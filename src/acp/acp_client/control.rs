//! The strict private runner control channel and its SDK transport relay.

use crate::acp::control_protocol::{
    self, ControlBody, NativeChannelAvailability, NativeChannelLossCause, NativeChannelLostMarker,
    PromptCompletedMarker, SessionReplayed,
};
use crate::acp::establishment::NativeRequestScope;
use crate::acp::state::{AcpConnectionCause, AcpConnectionSource, Event};
use agent_client_protocol::schema::v1::{PromptResponse, RequestId};
use agent_client_protocol::JsonRpcMessage as _;
use std::collections::HashMap;
use std::os::fd::{AsRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{mpsc, oneshot, Mutex};
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
use super::session_identity::{ControlIngressMetadata, SessionIngress, CONTROL_FRAME_BYTES_FIELD};

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
    pub(super) projection: Arc<crate::acp::event_sender::ProjectionState>,
    establishment_context: Arc<
        std::sync::Mutex<(
            Option<crate::session::ExecutionBinding>,
            Option<crate::acp::state::NativeObservationScope>,
        )>,
    >,
    application: Arc<std::sync::Mutex<Option<ApplicationSlot>>>,
    write: Arc<Mutex<tokio::net::unix::OwnedWriteHalf>>,
    handshake_rx: Mutex<mpsc::Receiver<ControlBody>>,
    native_cache: Arc<AtomicBool>,
    native_ready: Arc<AtomicBool>,
    pub(super) completion: Arc<std::sync::Mutex<PromptCompletion>>,
    /// A matched local outcome held until the crate reaches its
    /// [`PromptCompletedMarker`], or until one of the reader's relays fails.
    settled: Arc<std::sync::Mutex<Option<LocalOutcome>>>,
    raw_fd: RawFd,
    peer_identity: crate::acp::runner_lifecycle::RunnerIdentity,
    peer_process: crate::process::CapturedProcess,
    correlation: Arc<std::sync::Mutex<ShimCorrelation>>,
    reverse_changed: Arc<tokio::sync::Notify>,
}

struct ApplicationSlot {
    attempt: crate::acp::establishment::AttemptId,
    context: crate::acp::state::NativeEventContext,
    completed: Option<HandshakeApplication>,
}

pub(super) struct EstablishmentApplication {
    control: Arc<DaemonControlClient>,
    attempt: Option<crate::acp::establishment::AttemptId>,
}

impl EstablishmentApplication {
    pub(super) fn new(
        control: Arc<DaemonControlClient>,
    ) -> Result<Self, agent_client_protocol::Error> {
        let attempt = control
            .expected_attempt()
            .ok_or_else(|| acp_internal_error("establishment has no captured authority".into()))?;
        let context = control
            .establishment_context
            .lock()
            .expect("native establishment context")
            .1
            .as_ref()
            .map(|scope| scope.context)
            .or_else(|| control.projection.attempt().map(|attempt| attempt.context))
            .ok_or_else(|| {
                acp_internal_error("establishment has no captured physical context".into())
            })?;
        {
            let mut slot = control
                .application
                .lock()
                .expect("native response application");
            if slot.is_some() {
                return Err(acp_internal_error(
                    "previous original result is still being applied".into(),
                ));
            }
            *slot = Some(ApplicationSlot {
                attempt,
                context,
                completed: None,
            });
        }
        Ok(Self {
            control,
            attempt: Some(attempt),
        })
    }
}

impl Drop for EstablishmentApplication {
    fn drop(&mut self) {
        self.control.finish_establishment_projection(self.attempt);
    }
}

struct LocalOutcome {
    prompt_req_id: i64,
    correlation: Option<crate::acp::establishment::NativeOperationCorrelation>,
    tx: oneshot::Sender<ControlPromptResult>,
    result: ControlPromptResult,
}

#[derive(Debug, PartialEq)]
pub(super) enum ControlPromptResult {
    Outcome {
        outcome: control_protocol::PromptOutcome,
        evidence: Option<Box<control_protocol::NativePromptCompletionEvidence>>,
    },
    NativeLost,
}

struct HandshakeApplication(Arc<ConnectionState>);

impl std::fmt::Debug for HandshakeApplication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HandshakeApplication")
            .finish_non_exhaustive()
    }
}

impl Drop for HandshakeApplication {
    fn drop(&mut self) {
        self.0.finish_completed_outcome();
    }
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
    Adopted(Option<control_protocol::NativePromptHead>),
    Pending {
        // `None` until `PromptStarted`, so retained history read during send
        // cannot resolve this waiter.
        prompt_req_id: Option<i64>,
        scope: crate::acp::establishment::NativeRequestScope,
        window_id: Option<crate::acp::protocol::AcpPromptWindowId>,
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
    reverse: HashMap<i64, ReverseTicket>,
    forward: HashMap<ForwardKey, ForwardTicket>,
    /// Negative and descending so it never collides with a crate-minted id.
    next_synthetic: i64,
    reverse_exclusive: bool,
}

#[derive(Hash, PartialEq, Eq)]
enum ForwardKey {
    Sdk(RequestId),
    Runner(u64),
}

enum ForwardTicket {
    Sdk {
        call_id: u64,
        scope: NativeRequestScope,
    },
    Runner {
        sdk_id: RequestId,
    },
}

/// The relay owns duration until the runner settles its actual reverse write.
struct ReverseTicket {
    call_id: u64,
    _publication: Option<super::connection::CallbackDuration>,
    admitted: bool,
    reply_started: bool,
    callback: Option<Arc<super::session_identity::CapturedCallback>>,
}

pub(super) enum ReverseCallbackAdmission {
    Admitted(Arc<super::session_identity::CapturedCallback>),
    Excluded,
    Ended,
}

pub(super) struct ReverseQuiescence {
    correlation: Arc<std::sync::Mutex<ShimCorrelation>>,
    changed: Arc<tokio::sync::Notify>,
}

impl Drop for ReverseQuiescence {
    fn drop(&mut self) {
        self.correlation
            .lock()
            .expect("shim correlation")
            .reverse_exclusive = false;
        self.changed.notify_waiters();
    }
}

/// A pump exit is a real control end, including a terminal encoding failure.
struct EndControlPump(Arc<DaemonControlClient>);

impl Drop for EndControlPump {
    fn drop(&mut self) {
        self.0.shutdown();
    }
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
    if let Some(settled) = settled.lock().expect("settled mutex poisoned").take() {
        let _ = settled.tx.send(settled.result);
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TestRelayStage {
    Prepared,
    Relayed,
    Failed,
}

#[cfg(test)]
#[derive(Debug)]
pub(super) struct TestRelayObservation {
    pub(super) stage: TestRelayStage,
    pub(super) payload: serde_json::Value,
}

#[cfg(test)]
fn observe_test_relay<T: serde::Serialize + ?Sized>(
    state: &ConnectionState,
    value: &T,
    stage: TestRelayStage,
) {
    let observer = state.test_relay_observer.lock().unwrap();
    if let Some(observer) = observer.as_ref() {
        let payload = serde_json::to_value(value).expect("actual relay payload serializes");
        let _ = observer.send(TestRelayObservation { stage, payload });
    }
}
/// A failed relay releases any local outcome whose marker can no longer arrive.
async fn shim_write_line_releasing<T: serde::Serialize + ?Sized>(
    duplex: &Mutex<tokio::io::WriteHalf<tokio::io::DuplexStream>>,
    value: &T,
    settled: &std::sync::Mutex<Option<LocalOutcome>>,
    state: &ConnectionState,
) -> bool {
    #[cfg(test)]
    observe_test_relay(state, value, TestRelayStage::Prepared);
    if shim_write_line(duplex, value).await {
        #[cfg(test)]
        observe_test_relay(state, value, TestRelayStage::Relayed);
        return true;
    }
    state.observe(
        crate::acp::state::AcpConnectionSource::Control,
        crate::acp::state::AcpConnectionCause::ForwardPumpEnded,
    );
    release_settled(settled);
    state.mark_ordered_loss();
    #[cfg(test)]
    observe_test_relay(state, value, TestRelayStage::Failed);
    false
}

impl DaemonControlClient {
    /// Register the real SDK id before its output pump can consume the request.
    pub(super) fn register_forward<T>(
        &self,
        scope: NativeRequestScope,
        send: impl FnOnce() -> agent_client_protocol::SentRequest<T>,
    ) -> (agent_client_protocol::SentRequest<T>, u64) {
        let mut correlation = self.correlation.lock().expect("shim correlation");
        let call_id = correlation.forward_id();
        let sent = send();
        correlation.forward.insert(
            ForwardKey::Sdk(sent.id().clone()),
            ForwardTicket::Sdk { call_id, scope },
        );
        (sent, call_id)
    }

    pub(super) fn cancel_forward(&self, sdk_id: &RequestId, call_id: u64) {
        use std::collections::hash_map::Entry;

        let mut correlation = self.correlation.lock().expect("shim correlation");
        if let Entry::Occupied(entry) = correlation.forward.entry(ForwardKey::Sdk(sdk_id.clone())) {
            if matches!(entry.get(), ForwardTicket::Sdk { call_id: registered, .. } if *registered == call_id)
            {
                entry.remove();
                return;
            }
        }
        if let Entry::Occupied(entry) = correlation.forward.entry(ForwardKey::Runner(call_id)) {
            if matches!(entry.get(), ForwardTicket::Runner { sdk_id: registered } if registered == sdk_id)
            {
                entry.remove();
            }
        }
    }

    /// Move the existing ticket once; never capture permission at pump time.
    fn prepare_forward(
        &self,
        sdk_id: RequestId,
        method: String,
        params: serde_json::Value,
    ) -> Option<(RequestId, u64, ControlBody)> {
        let mut correlation = self.correlation.lock().expect("shim correlation");
        let ForwardTicket::Sdk { call_id, scope } = correlation
            .forward
            .remove(&ForwardKey::Sdk(sdk_id.clone()))?
        else {
            unreachable!("SDK keys contain only queued SDK tickets");
        };
        correlation.forward.insert(
            ForwardKey::Runner(call_id),
            ForwardTicket::Runner {
                sdk_id: sdk_id.clone(),
            },
        );
        Some((
            sdk_id,
            call_id,
            ControlBody::AgentCall {
                call_id,
                method,
                params,
                scope,
            },
        ))
    }

    pub(super) fn admit_reverse_callback(
        &self,
        id: &agent_client_protocol::schema::v1::RequestId,
    ) -> ReverseCallbackAdmission {
        let ticket = self.connection_state.admit(|| {
            let correlation = self.correlation.lock().expect("shim correlation");
            match id {
                agent_client_protocol::schema::v1::RequestId::Number(id) => correlation
                    .reverse
                    .get(id)
                    .map(|ticket| ticket.admitted.then(|| ticket.callback.clone()).flatten()),
                _ => None,
            }
        });
        match ticket {
            Ok(Some(Some(callback))) => ReverseCallbackAdmission::Admitted(callback),
            Ok(Some(None)) => ReverseCallbackAdmission::Excluded,
            Err(_) => ReverseCallbackAdmission::Ended,
            Ok(None) => {
                // A healthy relay must have reserved the actual ticket first.
                self.connection_state.observe(
                    AcpConnectionSource::Control,
                    AcpConnectionCause::ProtocolEnded,
                );
                self.shutdown();
                ReverseCallbackAdmission::Ended
            }
        }
    }

    pub(super) async fn quiesce_reverse(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<ReverseQuiescence, AcpError> {
        loop {
            let changed = self.reverse_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let ready = self.connection_state.admit(|| {
                let mut correlation = self.correlation.lock().expect("shim correlation");
                if correlation.reverse.is_empty() && !correlation.reverse_exclusive {
                    correlation.reverse_exclusive = true;
                    true
                } else {
                    false
                }
            })?;
            if ready {
                return Ok(ReverseQuiescence {
                    correlation: Arc::clone(&self.correlation),
                    changed: Arc::clone(&self.reverse_changed),
                });
            }
            tokio::select! {
                biased;
                _ = self.connection_state.wait_callback_end() => return Err(AcpError::NotRunning),
                result = tokio::time::timeout_at(deadline, changed) => {
                    result.map_err(|_| AcpError::ResetFailed(
                        "reset deadline expired while reverse replies were in flight".into(),
                    ))?;
                }
            }
        }
    }

    fn clear_correlation(&self) {
        {
            let mut correlation = self.correlation.lock().expect("shim correlation");
            correlation.forward.clear();
            correlation.reverse.clear();
        }
        self.reverse_changed.notify_waiters();
    }

    pub(super) fn set_establishment_context(
        &self,
        store: Option<crate::session::ExecutionBinding>,
        scope: Option<crate::acp::state::NativeObservationScope>,
    ) {
        *self
            .establishment_context
            .lock()
            .expect("native establishment context") = (store, scope);
    }

    pub(super) fn install_original_ingress(&self) -> agent_client_protocol::Result<()> {
        let Some(attempt) = self.projection.attempt() else {
            return Ok(());
        };
        let (store, constructor) = {
            let context = self
                .establishment_context
                .lock()
                .expect("native establishment context");
            (
                context.0.clone(),
                context
                    .1
                    .as_ref()
                    .map(|scope| (scope.context, Arc::clone(&scope.fence))),
            )
        };
        let postimage = self
            .projection
            .receipt_snapshot()
            .filter(|receipt| receipt.attempt.same_source(&attempt))
            .map(|receipt| receipt.postimage(attempt.context).map(Arc::new))
            .transpose()
            .map_err(|error| acp_internal_error(error.to_string()))?;
        if !self.ingress.authorize_original(
            Arc::clone(&attempt),
            self.peer_identity,
            store,
            postimage,
        ) {
            return Err(acp_internal_error(
                "original ingress custody is unavailable".into(),
            ));
        }
        if self.projection.original_window().is_some() {
            let (context, fence) = constructor.ok_or_else(|| {
                acp_internal_error("original constructor has no retained native fence".into())
            })?;
            if !self.ingress.authorize_constructor(&attempt, context, fence) {
                return Err(acp_internal_error(
                    "original constructor custody is unavailable".into(),
                ));
            }
        }
        Ok(())
    }

    pub(super) fn original_receipt(
        &self,
    ) -> Option<Arc<crate::acp::establishment::NativeEstablishmentReceipt>> {
        self.projection.receipt_snapshot()
    }

    pub(super) fn original_request_context(
        &self,
    ) -> Option<crate::acp::establishment::NativeRequestContext> {
        let receipt = self.projection.receipt_snapshot()?;
        let attempt = self.projection.attempt()?;
        if !receipt.attempt.same_source(&attempt) {
            return None;
        }
        let store = self
            .establishment_context
            .lock()
            .expect("native establishment context")
            .0
            .clone()
            .map(Arc::new);
        crate::acp::establishment::NativeRequestContext::new(
            crate::acp::establishment::NativeRequestScope::Original {
                attempt,
                owner: receipt.owner,
                store,
                operation: None,
            },
            None,
        )
        .with_original_evidence(receipt)
        .ok()
    }

    fn expected_attempt(&self) -> Option<crate::acp::establishment::AttemptId> {
        let context = self
            .establishment_context
            .lock()
            .expect("native establishment context");
        context
            .1
            .as_ref()
            .and_then(|scope| scope.choice.as_ref())
            .and_then(|choice| choice.authority.map(|key| key.id))
            .or_else(|| self.projection.attempt().map(|attempt| attempt.id))
    }

    fn finish_establishment_projection(
        &self,
        expected: Option<crate::acp::establishment::AttemptId>,
    ) {
        let mut application = self
            .application
            .lock()
            .expect("native response application");
        if application
            .as_ref()
            .is_some_and(|slot| Some(slot.attempt) == expected)
        {
            application.take();
        }
    }

    pub(super) fn applying_attempt(
        &self,
    ) -> Option<(
        crate::acp::establishment::AttemptId,
        crate::acp::state::NativeEventContext,
    )> {
        self.application
            .lock()
            .expect("native response application")
            .as_ref()
            .map(|slot| (slot.attempt, slot.context))
    }

    pub(super) fn shutdown(&self) {
        self.connection_state.observe(
            AcpConnectionSource::Control,
            AcpConnectionCause::ForwardPumpEnded,
        );
        // SAFETY: `self` keeps this exact socket alive for the call.
        unsafe { libc::shutdown(self.raw_fd, libc::SHUT_RDWR) };
        self.clear_correlation();
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
        let result = self.send_encoded(&wire, cached_management).await;
        #[cfg(test)]
        if result.is_ok() {
            if let Some(observer) = self
                .connection_state
                .test_control_output
                .lock()
                .unwrap()
                .as_ref()
            {
                let _ = observer.send(body);
            }
        }
        result
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
            Some(ControlBody::HandshakeFailed { method, error, .. }) if method == "initialize" => {
                Err(acp_error_from_value(error))
            }
            _ => Err(acp_internal_error(
                "control channel closed during initialize".into(),
            )),
        }
    }

    async fn establish_session(
        &self,
        method: &str,
        request: serde_json::Value,
    ) -> agent_client_protocol::Result<(String, Arc<serde_json::Value>)> {
        let attempt = self
            .projection
            .attempt()
            .filter(|attempt| attempt.target.accepts_request(method, &request))
            .ok_or_else(|| {
                acp_internal_error("native establishment has no matching durable attempt".into())
            })?;
        let store = self
            .establishment_context
            .lock()
            .expect("native establishment context")
            .0
            .clone();
        let expected_id = attempt.id;
        if !self.projection.rpc_may_be_issued(attempt.id) {
            return Err(acp_internal_error(
                "native establishment custody is sealed".into(),
            ));
        }
        if !self.ingress.authorize_original(
            Arc::clone(&attempt),
            self.peer_identity,
            store.clone(),
            None,
        ) {
            return Err(acp_internal_error(
                "original callback custody is unavailable".into(),
            ));
        }
        self.send(ControlBody::EstablishSession {
            method: method.to_string(),
            request,
            attempt: Arc::clone(&attempt),
            store: store.clone(),
        })
        .await
        .map_err(|e| acp_internal_error(format!("control write failed: {e}")))?;
        let Some(body) = self.receive_handshake().await else {
            return Err(acp_internal_error(
                "control channel closed during session establishment".into(),
            ));
        };
        match body {
            ControlBody::SessionReady {
                acp_session_id,
                result,
                receipt,
                ..
            } if receipt.attempt.id == expected_id
                && receipt.owner == self.peer_identity
                && native_stores_match(&receipt.store, &store)
                && receipt.attempt.same_source(&attempt) =>
            {
                Ok((acp_session_id, result))
            }
            ControlBody::EstablishmentFailed { receipt, error, .. }
                if receipt.attempt.id == expected_id
                    && receipt.owner == self.peer_identity
                    && native_stores_match(&receipt.store, &store)
                    && receipt.attempt.same_source(&attempt) =>
            {
                Err(acp_error_from_value(Arc::unwrap_or_clone(error)))
            }
            ControlBody::HandshakeFailed {
                method: failed,
                error,
                ..
            } if failed == method => Err(acp_error_from_value(error)),
            _ => Err(acp_internal_error(
                "control handshake did not match session establishment".into(),
            )),
        }
    }

    /// Obtain the runner's committed identity without touching agent session state.
    pub(super) async fn resume_session(&self) -> Result<String, agent_client_protocol::Error> {
        // Capture admission authority before requesting the immutable cached result.
        let expected = self
            .expected_attempt()
            .ok_or_else(|| acp_internal_error("resume has no captured native authority".into()))?;
        let store = self
            .establishment_context
            .lock()
            .expect("native establishment context")
            .0
            .clone();
        self.send(ControlBody::ResumeSession)
            .await
            .map_err(|e| acp_internal_error(format!("control write failed: {e}")))?;
        match self.receive_handshake().await {
            Some(ControlBody::SessionReady {
                acp_session_id,
                receipt,
                ..
            }) if receipt.attempt.id == expected
                && receipt.owner == self.peer_identity
                && native_stores_match(&receipt.store, &store) =>
            {
                Ok(acp_session_id)
            }
            Some(ControlBody::EstablishmentFailed { error, receipt, .. })
                if receipt.attempt.id == expected
                    && receipt.owner == self.peer_identity
                    && native_stores_match(&receipt.store, &store) =>
            {
                Err(acp_error_from_value(Arc::unwrap_or_clone(error)))
            }
            Some(ControlBody::HandshakeFailed { method, error, .. })
                if method == "resume_session" =>
            {
                Err(acp_error_from_value(error))
            }
            _ => Err(acp_internal_error(
                "control channel closed during resume".into(),
            )),
        }
    }

    pub(super) async fn session_replayed(&self) -> bool {
        self.projection.wait_wire().await
    }

    pub(super) fn mark_session_replayed(&self, marker: SessionReplayed) {
        self.projection
            .close_wire(marker.attempt_id, marker.closure, marker.delivery);
    }

    pub(super) fn deliver_prompt_completion(&self, marker: PromptCompletedMarker) {
        let mut settled = self.settled.lock().expect("settled mutex poisoned");
        if settled.as_ref().is_some_and(|outcome| {
            marker.prompt_req_id == Some(outcome.prompt_req_id)
                && marker.correlation == outcome.correlation
        }) {
            let outcome = settled.take().expect("matching original completion");
            let _ = outcome.tx.send(outcome.result);
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
        if matches!(*completion, PromptCompletion::Adopted(_)) {
            *completion = PromptCompletion::LocalIdle;
        }
    }

    /// Registers the completion waiter before sending the `Prompt` frame.
    pub(super) async fn prompt(
        &self,
        request: serde_json::Value,
        scope: crate::acp::establishment::NativeRequestScope,
    ) -> Result<oneshot::Receiver<ControlPromptResult>, AcpError> {
        self.connection_state.admit(|| ())?;
        let pending_scope = scope.clone();
        let wire = control_protocol::encode_frame(&ControlBody::Prompt { request, scope })
            .map_err(|error| {
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
                scope: pending_scope,
                window_id: None,
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

    pub(super) async fn cancel(
        &self,
        scope: crate::acp::establishment::NativeRequestScope,
    ) -> Result<(), AcpError> {
        self.send(ControlBody::Cancel { scope }).await
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

/// Dial one protocol6 socket and bind its actual peer before trusting Hello.
pub(super) async fn connect_runner_control(
    control_path: &std::path::Path,
    session_label: String,
    terminal_claim: Arc<TerminalClaim>,
    prompt_in_flight: Arc<std::sync::atomic::AtomicBool>,
    expected_owner: crate::acp::runner_lifecycle::RunnerIdentity,
    projection: Arc<crate::acp::event_sender::ProjectionState>,
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
        let prompt_head = match control_protocol::read_frame(&mut read_half).await {
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                session_id,
                prompt_head,
            })) if control_protocol_version == control_protocol::CONTROL_PROTOCOL_VERSION
                && session_id == session_label =>
            {
                prompt_head.filter(|head| {
                    head.scope.owner() == Some(expected_owner) && head.scope.correlation().is_some()
                })
            }
            Ok(Some(ControlBody::Hello {
                control_protocol_version,
                ..
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
        };
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
        Ok((read_half, write_half, peer_process, prompt_head))
    };
    let (mut read_half, write_half, peer_process, prompt_head) = tokio::time::timeout(bound, dial)
        .await
        .map_err(|_| unconfirmed())??;

    info!(
        target: "acp.protocol",
        session = %session_label,
        "runner control channel6 attached; runner owns the ACP protocol"
    );

    // 64 KiB matches the stdio pipe size.
    let (crate_side, shim_side) = tokio::io::duplex(64 * 1024);
    let (shim_read, shim_write) = tokio::io::split(shim_side);
    let shim_write = Arc::new(Mutex::new(shim_write));
    let correlation = Arc::new(std::sync::Mutex::new(ShimCorrelation::default()));
    let reverse_changed = Arc::new(tokio::sync::Notify::new());

    let raw_fd = write_half.as_ref().as_raw_fd();
    let write_half = Arc::new(Mutex::new(write_half));

    let reader_prompt_in_flight = prompt_in_flight.clone();
    let (hs_tx, hs_rx) = mpsc::channel::<ControlBody>(8);
    let completion = Arc::new(std::sync::Mutex::new(PromptCompletion::Adopted(
        prompt_head,
    )));
    let reader_completion = completion.clone();
    let settled = Arc::new(std::sync::Mutex::new(None));
    let reader_settled = settled.clone();
    let reader_session = session_label.clone();
    let reader_shim_write = shim_write.clone();
    let reader_correlation = correlation.clone();
    let reader_reverse_changed = Arc::clone(&reverse_changed);
    let ingress = Arc::new(SessionIngress::default());
    let connection_state = Arc::new(ConnectionState::default());
    #[cfg(test)]
    let _ = connection_state.test_shim.set(shim_write.clone());
    let reader_ingress = ingress.clone();
    let reader_connection_state = connection_state.clone();
    let native_cache = Arc::new(AtomicBool::new(false));
    let native_ready = Arc::new(AtomicBool::new(false));
    let reader_native_cache = native_cache.clone();
    let reader_native_ready = native_ready.clone();
    let reader_projection = Arc::clone(&projection);
    let application = Arc::new(std::sync::Mutex::new(None));
    let reader_application = Arc::clone(&application);
    let establishment_context = Arc::new(std::sync::Mutex::new((None, None)));
    let reader_establishment_context = Arc::clone(&establishment_context);
    let client = Arc::new(DaemonControlClient {
        correlation: Arc::clone(&correlation),
        reverse_changed,
        ingress,
        connection_state,
        projection,
        establishment_context,
        application,
        write: write_half,
        handshake_rx: Mutex::new(hs_rx),
        native_cache,
        native_ready,
        completion,
        settled,
        raw_fd,
        peer_identity: expected_owner,
        peer_process,
    });
    let _ = client.connection_state.control.set(Arc::downgrade(&client));
    #[cfg(test)]
    let _ = client
        .connection_state
        .test_terminal
        .set(Arc::clone(&terminal_claim));
    #[cfg(test)]
    let _ = client
        .connection_state
        .test_in_flight
        .set(Arc::clone(&prompt_in_flight));
    let reader_control = Arc::downgrade(&client);
    tokio::spawn(async move {
        async {
            let mut native_loss_relayed = false;
            loop {
                match control_protocol::read_frame_with_size(&mut read_half).await {
                    Ok(Some((ControlBody::OriginalEstablishment { invocation }, _))) => {
                        let context = reader_establishment_context.lock()
                            .expect("native establishment context").clone();
                        let expected_attempt = reader_projection.attempt();
                        let expected = context.1.as_ref()
                            .and_then(|scope: &crate::acp::state::NativeObservationScope| scope.choice.as_ref())
                            .and_then(|choice| choice.authority.map(|key| key.id))
                            .or_else(|| expected_attempt.as_ref().map(|attempt| attempt.id));
                        if expected != Some(invocation.attempt.id) {
                            continue;
                        }
                        // Original source survives rebase; this grants no Current permission.
                        if invocation.owner != expected_owner
                            || invocation.owner.birth_fingerprint.is_none()
                            || invocation.attempt.session_id != reader_session
                            || invocation.attempt.producer.is_none()
                            || expected_attempt.as_deref().is_none_or(|expected| {
                                !expected.same_source(&invocation.attempt)
                                    || expected.destination != invocation.attempt.destination
                            })
                            || !native_stores_match(&invocation.store, &context.0)
                            || invocation.replay.closed
                            || invocation.replay.lost
                            || !matches!(invocation.outcome,
                                crate::acp::establishment::NativeEstablishmentOutcome::Failed {
                                    reason: crate::acp::establishment::EstablishmentBlockReason::OutcomeUnknown,
                                })
                            || !reader_projection.observe_original_invocation(Arc::clone(&invocation.attempt))
                            || !reader_ingress.authorize_original(
                                Arc::clone(&invocation.attempt), invocation.owner,
                                invocation.store.clone(), None,
                            )
                        {
                            reader_projection.lost();
                            return;
                        }
                    }
                    Ok(Some((
                        frame @ (ControlBody::Initialized { .. }
                        | ControlBody::SessionReady { .. }
                        | ControlBody::HandshakeFailed { .. }
                        | ControlBody::EstablishmentFailed { .. }),
                        _,
                    ))) => {
                        let captured = match &frame {
                            ControlBody::SessionReady {
                                receipt, original_delivery, delivery,
                                acp_session_id, result, postimage, ..
                            } => {
                                let expected = receipt.postimage(receipt.attempt.context).ok();
                                if receipt.session_id() != Some(acp_session_id.as_str())
                                    || receipt.attempt.target.resulting_session_id(result).ok() != Some(acp_session_id.as_str())
                                    || postimage.as_deref() != expected.as_ref()
                                {
                                    reader_projection.lost();
                                    return;
                                }
                                Some((receipt, *original_delivery, *delivery))
                            }
                            ControlBody::EstablishmentFailed {
                                receipt, original_delivery, delivery, ..
                            } => Some((receipt, *original_delivery, *delivery)),
                            _ => None,
                        };
                        if let Some((receipt, original, delivery)) = captured {
                            let context = reader_establishment_context.lock()
                                .expect("native establishment context").clone();
                            let expected = context.1.as_ref().and_then(|scope| scope.choice.as_ref())
                                .and_then(|choice| choice.authority.map(|key| key.id))
                                .or_else(|| reader_projection.attempt().map(|attempt| attempt.id));
                            if expected != Some(receipt.attempt.id) {
                                // A delayed old frame cannot release or resolve the new attempt.
                                continue;
                            }
                            if receipt.owner != expected_owner
                                || receipt.owner.birth_fingerprint.is_none()
                                || receipt.attempt.session_id != reader_session
                                || receipt.attempt.producer.is_none()
                                || !native_stores_match(&receipt.store, &context.0)
                                || !reader_projection.receipt(Arc::clone(receipt), original, delivery)
                            {
                                reader_projection.lost();
                                return;
                            }
                            if receipt.replay.closed && !reader_connection_state.sdk_is_closing() {
                                let mut application = reader_application.lock()
                                    .expect("native response application");
                                if let Some(slot) = application.as_mut() {
                                    if slot.attempt == receipt.attempt.id && slot.completed.is_none() {
                                        reader_connection_state.begin_completed_outcome();
                                        slot.completed = Some(HandshakeApplication(Arc::clone(&reader_connection_state)));
                                    }
                                }
                            }
                        }
                        match &frame {
                            ControlBody::SessionReady { receipt, postimage, .. } => {
                                if !reader_ingress.authorize_original(
                                    Arc::clone(&receipt.attempt), receipt.owner,
                                    receipt.store.clone(), postimage.clone(),
                                ) && !reader_ingress.failed_original_matches(
                                    &receipt.attempt, receipt.owner, receipt.store.as_ref(),
                                ) {
                                    reader_projection.lost();
                                    return;
                                }
                            }
                            ControlBody::EstablishmentFailed { .. } => {
                                reader_ingress.revoke_requests();
                                reader_connection_state.decline_establishment();
                            }
                            _ => {}
                        }
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
                                    && !reader_ingress.has_failed()
                                    && reader_connection_state.loss().is_none(),
                                AtomicOrdering::Release,
                            );
                            reader_ingress.resolve(None, acp_session_id.clone().into());
                            #[cfg(test)]
                            if reader_native_ready.load(AtomicOrdering::Acquire) {
                                if let Some(observer) = reader_connection_state.test_native_session_seen.lock().unwrap().take() {
                                    let _ = observer.send(());
                                }
                            }
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
                            let mut correlation = reader_correlation.lock().expect("shim correlation");
                            correlation.forward.clear();
                            correlation.reverse.clear();
                        }
                        reader_reverse_changed.notify_waiters();
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
                    Ok(Some((ControlBody::PromptStarted { prompt_req_id, correlation, window_id }, _))) => {
                        let mut completion =
                            reader_completion.lock().expect("completion mutex poisoned");
                        if let PromptCompletion::Pending {
                            prompt_req_id: id @ None,
                            scope,
                            window_id: window,
                            ..
                        } = &mut *completion {
                            if scope.correlation() == correlation {
                                *id = Some(prompt_req_id);
                                *window = Some(window_id);
                            }
                        }
                    }
                    Ok(Some((
                        ControlBody::PromptCompleted {
                            prompt_req_id,
                            outcome,
                            evidence,
                        },
                        _,
                    ))) => {
                        #[cfg(test)]
                        {
                            let gate = reader_connection_state.test_prompt_completed.lock().unwrap().take();
                            if let Some((entered, release)) = gate {
                                let _ = entered.send(());
                                tokio::select! {
                                    biased;
                                    _ = reader_connection_state.wait_callback_end() => return,
                                    released = release => if released.is_err() { return; },
                                }
                            }
                        }
                        let evidence = if matches!(outcome, control_protocol::PromptOutcome::Aborted) {
                            None
                        } else { evidence };
                        let waiter = {
                            let mut completion =
                                reader_completion.lock().expect("completion mutex poisoned");
                            match &*completion {
                                PromptCompletion::Pending {
                                    prompt_req_id: Some(id),
                                    scope,
                                    window_id,
                                    ..
                                } if *id == prompt_req_id && evidence.as_ref().is_none_or(|evidence| {
                                    evidence.prompt_req_id == prompt_req_id
                                        && evidence.scope == *scope && Some(evidence.window_id) == *window_id
                                }) => {
                                    match std::mem::replace(
                                        &mut *completion,
                                        PromptCompletion::LocalIdle,
                                    ) {
                                        PromptCompletion::Pending { tx, scope, .. } => Some((tx, scope.correlation())),
                                        _ => unreachable!(),
                                    }
                                }
                                _ => None,
                            }
                        };
                        if let Some((tx, correlation)) = waiter {
                            *reader_settled.lock().expect("settled mutex poisoned") = Some(LocalOutcome {
                                prompt_req_id,
                                correlation,
                                tx,
                                result: ControlPromptResult::Outcome { outcome, evidence },
                            });
                            let params = PromptCompletedMarker {
                                prompt_req_id: Some(prompt_req_id),
                                correlation,
                                ..PromptCompletedMarker::default()
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
                        else if let Some(adopted) = prepare_adopted_completion(
                            prompt_req_id, outcome, evidence, &reader_completion,
                            &reader_prompt_in_flight, &terminal_claim,
                        ) {
                            let params = PromptCompletedMarker {
                                adopted: Some(Box::new(adopted)),
                                ..PromptCompletedMarker::default()
                            };
                            let marker = CompletionMarkerLine {
                                jsonrpc: "2.0",
                                method: params.method(),
                                params: &params,
                            };
                            if !shim_write_line_releasing(
                                &reader_shim_write, &marker, &reader_settled,
                                &reader_connection_state,
                            ).await {
                                return;
                            }
                        }
                    }
                    // Refuse invalid scopes without waiting behind Fresh or
                    // blocked SDK publication; duration capture never waits.
                    Ok(Some((
                        ControlBody::ServerCall {
                            call_id,
                            method,
                            params,
                            evidence,
                        },
                        _,
                    ))) => {
                        let callback = reader_ingress.capture_native_callback(
                            evidence,
                            params.get("sessionId").or_else(|| params.get(0))
                                .and_then(serde_json::Value::as_str),
                            &reader_connection_state,
                        );
                        if callback.is_none()
                            && (control_protocol::session_scoped_callback(&method, &params)
                                || method == "elicitation/create")
                        {
                            let Some(control) = reader_control.upgrade() else {
                                return;
                            };
                            if control.send(ControlBody::ServerError {
                                call_id,
                                error: control_protocol::JsonRpcError {
                                    code: control_protocol::INVALID_PARAMS,
                                    message: SessionIngress::request_refused().message,
                                    data: None,
                                },
                            }).await.is_err() {
                                return;
                            }
                            continue;
                        }
                        let synthetic = reader_connection_state.admit(|| {
                            let mut c = reader_correlation.lock().expect("shim correlation");
                            let id = c.synthetic_id();
                            let publication = if !c.reverse_exclusive && callback.is_some() {
                                reader_connection_state.callback_publication()
                            } else {
                                None
                            };
                            let admitted = publication.is_some();
                            c.reverse.insert(id, ReverseTicket {
                                call_id,
                                _publication: publication,
                                admitted,
                                reply_started: false,
                                callback,
                            });
                            id
                        });
                        let Ok(synthetic) = synthetic else {
                            continue;
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
                    Ok(Some((ControlBody::Notify { method, mut params, replay, evidence }, wire_bytes))) => {
                        if let Some(replay) = &replay {
                            // Account for rejected replay too, but never expose a
                            // known-incomplete prefix as committed Native history.
                            if !reader_projection.frame(replay) {
                                continue;
                            }
                        }
                        // Only this daemon mints completion barriers and their classified payload.
                        if PromptCompletedMarker::matches_method(&method)
                            || NativeChannelLostMarker::matches_method(&method)
                        {
                            continue;
                        }
                        let replay_closed: Option<SessionReplayed> =
                            if SessionReplayed::matches_method(&method) {
                                if replay.is_some() {
                                    reader_projection.lost();
                                    continue;
                                }
                                match serde::Deserialize::deserialize(&params) {
                                    Ok(marker) => Some(marker),
                                    Err(_) => {
                                        reader_projection.lost();
                                        return;
                                    }
                                }
                            } else {
                                None
                            };
                        if method == "session/update" || method == crate::acp::state::AUTH_STATUS_UPDATE_METHOD {
                            let metadata = serde_json::to_value(ControlIngressMetadata { wire_bytes, evidence })
                                .expect("private ingress metadata serializes");
                            match &mut params {
                                serde_json::Value::Object(fields) => {
                                    fields.insert(CONTROL_FRAME_BYTES_FIELD.into(), metadata);
                                }
                                serde_json::Value::Array(fields) => fields.push(metadata),
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
                        if let Some(marker) = replay_closed {
                            reader_projection.wire_forwarded(&marker);
                        }
                    }
                    Ok(Some((ControlBody::ServerCallSettled { call_id }, _))) => {
                        let removed = {
                            let mut correlation = reader_correlation.lock().expect("shim correlation");
                            let synthetic = correlation.reverse.iter().find_map(|(id, ticket)| {
                                (ticket.call_id == call_id && ticket.reply_started).then_some(*id)
                            });
                            synthetic.and_then(|id| correlation.reverse.remove(&id))
                        };
                        if removed.is_some() {
                            // Socket send alone cannot release Fresh's duration fence:
                            // the runner must finish its per-write source check first.
                            drop(removed);
                            if let Some(control) = reader_control.upgrade() {
                                control.reverse_changed.notify_waiters();
                            }
                        }
                    }
                    // Forward lane: resolve the crate's own pending request.
                    Ok(Some((ControlBody::AgentResult { call_id, result }, _))) => {
                        let ticket = reader_correlation.lock().expect("shim correlation")
                            .forward.remove(&ForwardKey::Runner(call_id));
                        if let Some(ForwardTicket::Runner { sdk_id }) = ticket {
                            let line = serde_json::json!({
                                "jsonrpc": "2.0", "id": sdk_id, "result": result,
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
                        let ticket = reader_correlation.lock().expect("shim correlation")
                            .forward.remove(&ForwardKey::Runner(call_id));
                        if let Some(ForwardTicket::Runner { sdk_id }) = ticket {
                            let line = serde_json::json!({
                                "jsonrpc": "2.0", "id": sdk_id, "error": error,
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
        reader_projection.control_ended();
        reader_connection_state.observe(
            crate::acp::state::AcpConnectionSource::Control,
            crate::acp::state::AcpConnectionCause::ForwardPumpEnded,
        );
        {
            let mut correlation = reader_correlation.lock().expect("shim correlation");
            correlation.forward.clear();
            correlation.reverse.clear();
        }
        reader_reverse_changed.notify_waiters();
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
        let _end = EndControlPump(Arc::clone(&pump_control));
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
            #[cfg(test)]
            if let Some(observer) = pump_connection_state
                .test_sdk_output
                .lock()
                .unwrap()
                .as_ref()
            {
                let _ = observer.send(value.clone());
            }
            let mut forward = None;
            #[cfg(test)]
            let mut reverse_reply = false;
            let frame = if let Some(method) = value
                .get("method")
                .and_then(|method| method.as_str())
                .map(str::to_owned)
            {
                let Some(id) = value.get_mut("id").map(std::mem::take) else {
                    continue;
                };
                let Ok(sdk_id) = serde_json::from_value::<RequestId>(id) else {
                    continue;
                };
                let params = value
                    .get_mut("params")
                    .map(std::mem::take)
                    .unwrap_or(serde_json::Value::Null);
                let prepared = pump_connection_state
                    .admit(|| pump_control.prepare_forward(sdk_id, method, params));
                let Ok(Some((sdk_id, call_id, frame))) = prepared else {
                    continue;
                };
                forward = Some((sdk_id, call_id));
                frame
            } else {
                let Some(synthetic) = value.get("id").and_then(|id| id.as_i64()) else {
                    continue;
                };
                let Some(call_id) = ({
                    let mut correlation = pump_correlation.lock().expect("shim correlation");
                    correlation.reverse.get_mut(&synthetic).and_then(|ticket| {
                        if ticket.reply_started {
                            None
                        } else {
                            ticket.reply_started = true;
                            Some(ticket.call_id)
                        }
                    })
                }) else {
                    continue;
                };
                #[cfg(test)]
                {
                    reverse_reply = true;
                }
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
                    if let Some((id, call_id)) = forward.take() {
                        pump_control.cancel_forward(&id, call_id);
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
            if let Some((_, call_id)) = &forward {
                let registered = pump_connection_state.admit(|| {
                    pump_correlation
                        .lock()
                        .expect("shim correlation")
                        .forward
                        .contains_key(&ForwardKey::Runner(*call_id))
                });
                if !matches!(registered, Ok(true)) {
                    continue;
                }
            }
            #[cfg(test)]
            if reverse_reply {
                let gate = pump_connection_state
                    .test_reverse_send
                    .lock()
                    .unwrap()
                    .take();
                if let Some((entered, release)) = gate {
                    let _ = entered.send(());
                    tokio::select! {
                        biased;
                        _ = pump_connection_state.wait_callback_end() => return,
                        released = release => if released.is_err() { return; },
                    }
                }
            }
            if pump_control.send_encoded(&wire, false).await.is_err() {
                if let Some((sdk_id, call_id)) = forward {
                    pump_control.cancel_forward(&sdk_id, call_id);
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

fn matches_adopted_head(
    head: &control_protocol::NativePromptHead,
    prompt_req_id: i64,
    evidence: Option<&control_protocol::NativePromptCompletionEvidence>,
) -> bool {
    head.prompt_req_id == prompt_req_id
        && evidence.is_none_or(|evidence| {
            evidence.scope == head.scope && evidence.window_id == head.window_id
        })
}

fn completion_correlation(
    evidence: &control_protocol::NativePromptCompletionEvidence,
    peer: crate::acp::runner_lifecycle::RunnerIdentity,
) -> Option<crate::acp::establishment::NativeOperationCorrelation> {
    use crate::acp::establishment::NativeRequestScope;
    let receipt = &evidence.receipt;
    if receipt.owner != peer
        || peer.birth_fingerprint.is_none()
        || receipt.attempt.producer.is_none()
        || !receipt.replay.closed
        || receipt.replay.lost
        || !receipt.session_id().is_some_and(|sid| !sid.is_empty())
        || evidence.scope.owner() != Some(peer)
        || evidence.scope.origin(Some(receipt)) != Some(evidence.origin)
    {
        return None;
    }
    match &evidence.scope {
        NativeRequestScope::Operation { permission } => {
            if receipt
                .postimage(permission.session.postimage.context)
                .ok()?
                != permission.session.postimage
            {
                return None;
            }
        }
        NativeRequestScope::Original {
            attempt,
            owner,
            store,
            ..
        } => {
            if attempt.as_ref() != receipt.attempt.as_ref()
                || *owner != receipt.owner
                || !receipt.same_store(store.as_deref())
            {
                return None;
            }
        }
        _ => return None,
    }
    evidence.scope.correlation()
}

fn prepare_adopted_completion(
    prompt_req_id: i64,
    outcome: control_protocol::PromptOutcome,
    evidence: Option<Box<control_protocol::NativePromptCompletionEvidence>>,
    completion: &std::sync::Mutex<PromptCompletion>,
    in_flight: &std::sync::atomic::AtomicBool,
    terminal: &TerminalClaim,
) -> Option<control_protocol::AdoptedPromptCompletion> {
    use control_protocol::AdoptedPromptOutcomeKind;
    let outcome_kind = match &outcome {
        control_protocol::PromptOutcome::Completed { .. } => AdoptedPromptOutcomeKind::Completed,
        control_protocol::PromptOutcome::Error { .. } => AdoptedPromptOutcomeKind::Error,
        control_protocol::PromptOutcome::Aborted => AdoptedPromptOutcomeKind::Aborted,
    };
    let turn = {
        let completion = completion.lock().expect("completion mutex poisoned");
        match &*completion {
            PromptCompletion::Adopted(Some(head))
                if matches_adopted_head(head, prompt_req_id, evidence.as_deref())
                    && in_flight.load(AtomicOrdering::Acquire) =>
            {
                Some(terminal.current_turn())
            }
            _ => None,
        }
    };
    if turn.is_none()
        && (evidence.is_none() || matches!(outcome_kind, AdoptedPromptOutcomeKind::Aborted))
    {
        return None;
    }
    let (diagnostic, reason) = adopted_prompt_completion(outcome);
    Some(control_protocol::AdoptedPromptCompletion {
        turn,
        prompt_req_id,
        outcome_kind,
        diagnostic,
        reason,
        evidence,
    })
}

pub(super) enum AdoptedCompletionPublication {
    Rejected,
    Historical,
    Settled(u64),
}

pub(super) async fn publish_adopted_completion(
    adopted: control_protocol::AdoptedPromptCompletion,
    events: &crate::acp::event_sender::EventSender,
    control: &DaemonControlClient,
    in_flight: &std::sync::atomic::AtomicBool,
    terminal: &TerminalClaim,
    calendar_hint: impl FnOnce() -> Option<chrono::DateTime<chrono::Utc>>,
) -> AdoptedCompletionPublication {
    use crate::acp::event_sender::EventMetadata;
    use crate::acp::protocol::{
        AcpOperationMetadata, AcpOriginalRequestResult, AcpPromptObservation,
    };
    use control_protocol::AdoptedPromptOutcomeKind;
    let control_protocol::AdoptedPromptCompletion {
        turn,
        prompt_req_id,
        outcome_kind,
        mut diagnostic,
        reason,
        evidence,
    } = adopted;
    let correlation = evidence
        .as_deref()
        .and_then(|evidence| completion_correlation(evidence, control.peer_identity));
    if evidence
        .as_ref()
        .is_some_and(|evidence| evidence.prompt_req_id != prompt_req_id)
        || (evidence.is_some() && correlation.is_none())
    {
        return AdoptedCompletionPublication::Rejected;
    }
    let native_outcome = match (evidence.as_ref(), outcome_kind) {
        (Some(_), AdoptedPromptOutcomeKind::Completed) => Some(AcpOriginalRequestResult::Succeeded),
        (Some(_), AdoptedPromptOutcomeKind::Error) => Some(AcpOriginalRequestResult::Failed),
        _ => None,
    };
    let local_correlation = {
        let completion = control
            .completion
            .lock()
            .expect("completion mutex poisoned");
        match &*completion {
            PromptCompletion::Adopted(Some(head))
                if turn.is_some()
                    && matches_adopted_head(head, prompt_req_id, evidence.as_deref()) =>
            {
                head.scope.correlation()
            }
            _ => None,
        }
    };
    let mut metadata = EventMetadata::history();
    if let Some(outcome) = native_outcome {
        let evidence = evidence.as_ref().expect("qualified Native result");
        metadata.native_origin = Some(evidence.origin);
        metadata.native_prompt_id = Some(evidence.prompt_req_id);
        metadata.operation = Some(AcpOperationMetadata {
            correlation: correlation.expect("validated original correlation"),
            outcome: Some(outcome),
        });
        metadata.prompt_observation = Some(AcpPromptObservation::Written {
            window_id: evidence.window_id,
        });
        metadata.output_uncertainty = evidence.uncertain_output.then_some(evidence.window_id);
    } else if let Some(correlation) = local_correlation {
        metadata.operation = Some(AcpOperationMetadata {
            correlation,
            outcome: None,
        });
    } else {
        return AdoptedCompletionPublication::Rejected;
    }
    let sender = events.with_metadata(metadata);
    let count = 1 + usize::from(diagnostic.is_some());
    let Ok(mut permits) = sender.reserve_many(count).await else {
        return AdoptedCompletionPublication::Rejected;
    };
    let completion = control
        .completion
        .lock()
        .expect("completion mutex poisoned");
    let local_match = matches!(&*completion, PromptCompletion::Adopted(Some(head))
        if matches_adopted_head(head, prompt_req_id, evidence.as_deref()))
        && in_flight.load(AtomicOrdering::Acquire);
    if local_match {
        if let Some(diagnostic) = &mut diagnostic {
            diagnostic.inherit_rate_limit_hint(calendar_hint);
        }
    }
    let failed = matches!(reason.as_str(), "prompt_failed" | "rate_limited");
    let mut reason = Some(reason);
    let mut publish = || {
        if let Some(event) = diagnostic.take() {
            permits
                .next()
                .expect("diagnostic permit")
                .send(event.into_event());
        }
        permits
            .next()
            .expect("terminal permit")
            .send(Event::Stopped {
                reason: reason.take().expect("terminal reason"),
            });
    };
    if let Some(turn) = turn.filter(|_| local_match) {
        if terminal.claim_epoch(turn, failed)
            && terminal.publish_claimed(turn, failed, || {
                publish();
                in_flight.store(false, AtomicOrdering::Release);
            })
        {
            return AdoptedCompletionPublication::Settled(turn);
        }
    }
    if native_outcome.is_some() {
        publish();
        AdoptedCompletionPublication::Historical
    } else {
        AdoptedCompletionPublication::Rejected
    }
}

/// Same result shape as the crate `send_request` it replaces.
pub(super) async fn establish_session<Resp: serde::de::DeserializeOwned>(
    control: &DaemonControlClient,
    method: &str,
    request: &impl serde::Serialize,
) -> agent_client_protocol::Result<Resp> {
    let params = serde_json::to_value(request)
        .map_err(|e| acp_internal_error(format!("serialize {method} params: {e}")))?;
    let (_id, result) = control.establish_session(method, params).await?;
    serde::Deserialize::deserialize(result.as_ref())
        .map_err(|e| acp_internal_error(format!("deserialize {method} result: {e}")))
}

fn native_stores_match(
    left: &Option<crate::session::ExecutionBinding>,
    right: &Option<crate::session::ExecutionBinding>,
) -> bool {
    left == right
        && left.as_ref().map(|store| store.exported_default_store)
            == right.as_ref().map(|store| store.exported_default_store)
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
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};

    struct ManagedControlFixture {
        client: crate::acp::supervisor::test_support::managed::ManagedClient,
        native: crate::acp::supervisor::test_support::managed::NativePeer,
        prompts: mpsc::UnboundedReceiver<serde_json::Value>,
        store: Arc<crate::acp::event_store::EventStore>,
    }

    impl ManagedControlFixture {
        async fn new(
            scope: &mut crate::acp::supervisor::test_support::managed::ManagedScope,
        ) -> Self {
            let directory = scope.tempdir();
            let session_id = format!("control-native-{}", uuid::Uuid::new_v4());
            let (seen, prompts) = mpsc::unbounded_channel();
            let (config, native) =
                scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
                    let id = request.get("id")?;
                    match request["method"].as_str().unwrap() {
                        "initialize" => Some(serde_json::json!({
                            "jsonrpc":"2.0","id":id,
                            "result":{"protocolVersion":1,"agentCapabilities":{}}
                        })),
                        "session/new" => Some(serde_json::json!({
                            "jsonrpc":"2.0","id":id,"result":{"sessionId":session_id}
                        })),
                        "session/prompt" => {
                            assert_eq!(request["params"]["sessionId"], session_id);
                            seen.send(request.clone()).unwrap();
                            None
                        }
                        "session/cancel" => None,
                        method => panic!("unexpected native operation: {method}"),
                    }
                });
            let store = Arc::clone(&scope.state.acp_event_store);
            let client = scope.spawn(config, "control-contract").await.unwrap();
            client.ready().await;
            Self {
                client,
                native,
                prompts,
                store,
            }
        }

        async fn start_prompt(&mut self, text: &str) -> serde_json::Value {
            self.client.send_prompt(text, &[]).await.unwrap();
            tokio::time::timeout(Duration::from_secs(10), self.prompts.recv())
                .await
                .expect("actual Native prompt received")
                .expect("Native peer remains live")
        }

        async fn adopt_in_flight(
            self,
            scope: &mut crate::acp::supervisor::test_support::managed::ManagedScope,
        ) -> Self {
            let Self {
                client,
                native,
                prompts,
                store,
            } = self;
            let client = scope.adopt(client, true).await;
            client.ready().await;
            Self {
                client,
                native,
                prompts,
                store,
            }
        }

        async fn next_event(&mut self) -> Event {
            tokio::time::timeout(Duration::from_secs(10), self.client.next_event())
                .await
                .expect("actual SDK publication before deadline")
                .expect("observer remains live")
        }

        async fn complete(&self, request: &serde_json::Value, reason: &str) {
            self.native
                .send(serde_json::json!({
                    "jsonrpc":"2.0","id":request["id"],"result":{"stopReason":reason}
                }))
                .await;
        }
    }
    use std::future::Future;

    async fn real_control_deadline<F: Future>(future: F) -> F::Output {
        tokio::time::timeout(Duration::from_secs(15), future)
            .await
            .expect("actual managed control operation completes")
    }

    fn observe_real_relay(
        control: &DaemonControlClient,
    ) -> mpsc::UnboundedReceiver<TestRelayObservation> {
        let (observer, observed) = mpsc::unbounded_channel();
        assert!(control
            .connection_state
            .test_relay_observer
            .lock()
            .unwrap()
            .replace(observer)
            .is_none());
        observed
    }

    async fn real_relay_observation(
        observed: &mut mpsc::UnboundedReceiver<TestRelayObservation>,
        stage: TestRelayStage,
        method: &str,
    ) -> serde_json::Value {
        real_control_deadline(async {
            loop {
                let observation = observed
                    .recv()
                    .await
                    .expect("actual relay remains observed");
                if observation.stage == stage && observation.payload["method"] == method {
                    return observation.payload;
                }
            }
        })
        .await
    }

    async fn real_completion_marker(
        observed: &mut mpsc::UnboundedReceiver<TestRelayObservation>,
        stage: TestRelayStage,
        request: &serde_json::Value,
    ) -> PromptCompletedMarker {
        let method = PromptCompletedMarker::default().method().to_owned();
        let payload = real_relay_observation(observed, stage, &method).await;
        let marker: PromptCompletedMarker =
            serde_json::from_value(payload["params"].clone()).unwrap();
        let id = marker
            .prompt_req_id
            .or_else(|| marker.adopted.as_ref().map(|adopted| adopted.prompt_req_id));
        assert_eq!(id, request["id"].as_i64());
        marker
    }

    async fn real_pending_scope(
        control: &DaemonControlClient,
        request: &serde_json::Value,
    ) -> (
        crate::acp::establishment::NativeRequestScope,
        crate::acp::protocol::AcpPromptWindowId,
    ) {
        let id = request["id"]
            .as_i64()
            .expect("actual native numeric request id");
        real_control_deadline(async {
            loop {
                let snapshot = {
                    let completion = control.completion.lock().unwrap();
                    match &*completion {
                        PromptCompletion::Pending {
                            prompt_req_id: Some(pending),
                            scope,
                            window_id: Some(window),
                            ..
                        } if *pending == id => Some((scope.clone(), *window)),
                        _ => None,
                    }
                };
                if let Some(snapshot) = snapshot {
                    assert_eq!(snapshot.0.owner(), Some(control.peer_identity()));
                    assert!(snapshot.0.correlation().is_some());
                    return snapshot;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn managed_prompt_admission_waits_for_the_same_frozen_native_scope() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut fixture = ManagedControlFixture::new(scope).await;
                let actual = fixture.client.actual();
                let state = actual.test_connection_state();
                let control = actual.test_control();
                let receipt = actual
                    .establishment_projection()
                    .unwrap()
                    .receipt_snapshot()
                    .unwrap();
                let (prepared, prepared_wait) = oneshot::channel();
                let (release, release_wait) = oneshot::channel();
                let (busy, busy_wait) = oneshot::channel();
                assert!(state
                    .test_prompt_admission
                    .lock()
                    .unwrap()
                    .replace((prepared, release_wait))
                    .is_none());
                assert!(state
                    .test_prompt_admission_busy
                    .lock()
                    .unwrap()
                    .replace(busy)
                    .is_none());
                fixture
                    .client
                    .send_prompt("one frozen original across actual contention", &[])
                    .await
                    .unwrap();
                real_control_deadline(prepared_wait).await.unwrap();
                let storage = crate::session::Storage::open_reference("test").unwrap();
                let exclusion = storage
                    .acquire_instance_lifecycle_lock(&fixture.client.id)
                    .unwrap();
                release.send(()).unwrap();
                real_control_deadline(busy_wait).await.unwrap();
                assert!(
                    state.admission.try_lock().is_ok(),
                    "no admission mutex is retained across the physical wait"
                );
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), fixture.prompts.recv())
                        .await
                        .is_err(),
                    "actual RegistryBusy was observed before this no-write window"
                );
                assert_eq!(
                    fixture
                        .native
                        .requests()
                        .iter()
                        .filter(|method| method.as_str() == "session/prompt")
                        .count(),
                    0
                );
                drop(exclusion);
                let request = real_control_deadline(fixture.prompts.recv()).await.unwrap();
                let (original_scope, window) = real_pending_scope(&control, &request).await;
                assert_eq!(original_scope.owner(), Some(receipt.owner));
                assert_eq!(
                    actual
                        .establishment_projection()
                        .unwrap()
                        .receipt_snapshot()
                        .unwrap()
                        .as_ref(),
                    receipt.as_ref()
                );
                fixture.complete(&request, "end_turn").await;
                real_events_until_stopped(&mut fixture, "prompt_complete").await;
                assert_one_real_stopped(&fixture, 0, &request, &original_scope, window);
                let methods = fixture.native.requests();
                assert_eq!(
                    methods
                        .iter()
                        .filter(|method| method.as_str() == "session/new")
                        .count(),
                    1
                );
                assert_eq!(
                    methods
                        .iter()
                        .filter(|method| method.as_str() == "session/prompt")
                        .count(),
                    1
                );
                assert!(!methods.iter().any(|method| method == "session/load"));
                fixture.client.shutdown().await.unwrap();
            })
        })
        .await;
    }

    fn assert_real_settled(
        control: &DaemonControlClient,
        request: &serde_json::Value,
        scope: &crate::acp::establishment::NativeRequestScope,
        window: crate::acp::protocol::AcpPromptWindowId,
    ) {
        let settled = control.settled.lock().unwrap();
        let settled = settled
            .as_ref()
            .expect("actual correlated result is held for its marker");
        assert_eq!(Some(settled.prompt_req_id), request["id"].as_i64());
        assert_eq!(settled.correlation, scope.correlation());
        match &settled.result {
            ControlPromptResult::Outcome {
                evidence: Some(evidence),
                ..
            } => {
                assert_eq!(Some(evidence.prompt_req_id), request["id"].as_i64());
                assert_eq!(&evidence.scope, scope);
                assert_eq!(evidence.window_id, window);
                assert_eq!(evidence.receipt.owner, control.peer_identity());
                assert_eq!(
                    evidence.receipt.session_id(),
                    request["params"]["sessionId"].as_str()
                );
            }
            other => panic!("actual native result must retain real evidence: {other:?}"),
        }
    }

    async fn real_events_until_stopped(
        fixture: &mut ManagedControlFixture,
        expected_reason: &str,
    ) -> Vec<Event> {
        real_control_deadline(async {
            let mut events = Vec::new();
            loop {
                let event = fixture.next_event().await;
                assert!(
                    !matches!(
                        &event,
                        Event::AgentStartupError { .. } | Event::AcpConnectionUnavailable { .. }
                    ),
                    "live native operation unexpectedly failed: {event:?}"
                );
                assert!(!serde_json::to_string(&event)
                    .unwrap()
                    .contains("privateTokenMarker"));
                let stopped = if let Event::Stopped { reason } = &event {
                    assert_eq!(reason, expected_reason);
                    true
                } else {
                    false
                };
                events.push(event);
                if stopped {
                    return events;
                }
            }
        })
        .await
    }

    fn assert_one_real_stopped(
        fixture: &ManagedControlFixture,
        after: u64,
        request: &serde_json::Value,
        scope: &crate::acp::establishment::NativeRequestScope,
        window: crate::acp::protocol::AcpPromptWindowId,
    ) {
        let stored = fixture
            .store
            .replay_from(&fixture.client.id, after)
            .unwrap();
        let stops: Vec<_> = stored
            .iter()
            .filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. }))
            .collect();
        assert_eq!(
            stops.len(),
            1,
            "one publication belongs to this original request"
        );
        let metadata = &stops[0].metadata;
        assert_eq!(metadata.native_prompt_id, request["id"].as_i64());
        assert_eq!(
            metadata.operation.map(|operation| operation.correlation),
            scope.correlation()
        );
        assert_eq!(
            metadata.prompt_observation,
            Some(crate::acp::protocol::AcpPromptObservation::Written { window_id: window })
        );
        assert!(metadata.native_origin.is_some());
    }

    #[derive(Clone, Copy, Debug)]
    enum RealAdoptedOutcome {
        Completed,
        Error,
        Quota,
    }

    impl RealAdoptedOutcome {
        fn reason(self) -> &'static str {
            match self {
                Self::Completed => "prompt_complete",
                Self::Error => "prompt_failed",
                Self::Quota => "rate_limited",
            }
        }

        async fn reply(self, fixture: &ManagedControlFixture, request: &serde_json::Value) {
            if matches!(self, Self::Quota) {
                fixture.native.send(serde_json::json!({
                    "jsonrpc":"2.0", "method":"session/update", "params":{
                        "sessionId":request["params"]["sessionId"],
                        "update":{"sessionUpdate":"usage_update","used":100,"size":200,
                            "_meta":{"_claude/rateLimit":{
                                "status":"rejected","rateLimitType":"five_hour","resetsAt":4_102_444_800_i64
                            }}
                        }
                    }
                })).await;
            }
            let response = match self {
                Self::Completed => serde_json::json!({
                    "jsonrpc":"2.0", "id":request["id"], "result":{"stopReason":"end_turn"}
                }),
                Self::Error => serde_json::json!({
                    "jsonrpc":"2.0", "id":request["id"], "error":{
                        "code":-32603, "message":"original request failed",
                        "data":{"errorKind":"provider_error", "privateTokenMarker":"must-not-leak"}
                    }
                }),
                Self::Quota => serde_json::json!({
                    "jsonrpc":"2.0", "id":request["id"], "error":{
                        "code":-32000, "message":"rate limit exceeded",
                        "data":{"errorKind":"rate_limit", "privateTokenMarker":"must-not-leak"}
                    }
                }),
            };
            fixture.native.send(response).await;
        }
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn managed_adopted_original_outcomes_preserve_the_next_explicit_input() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                for case in [
                    RealAdoptedOutcome::Completed,
                    RealAdoptedOutcome::Error,
                    RealAdoptedOutcome::Quota,
                ] {
                    let mut fixture = ManagedControlFixture::new(scope).await;
                    let request = fixture.start_prompt("real original before adoption").await;
                    let mut fixture = fixture.adopt_in_flight(scope).await;
                    let actual = fixture.client.actual();
                    let control = actual.test_control();
                    let state = actual.test_connection_state();
                    let terminal = state.test_terminal.get().unwrap().clone();
                    let in_flight = state.test_in_flight.get().unwrap().clone();
                    let head = match &*control.completion.lock().unwrap() {
                        PromptCompletion::Adopted(Some(head)) => head.clone(),
                        _ => panic!("actual adoption retains its genuine original head"),
                    };
                    assert_eq!(Some(head.prompt_req_id), request["id"].as_i64());
                    assert_eq!(head.scope.owner(), Some(control.peer_identity()));
                    assert!(head.scope.correlation().is_some());
                    assert_ne!(control.peer_identity().pid, std::process::id());
                    let turn = terminal.current_turn();
                    let before = fixture.store.highest_seq(&fixture.client.id);
                    assert!(!terminal.claimed());
                    assert!(in_flight.load(AtomicOrdering::Acquire));
                    case.reply(&fixture, &request).await;
                    let events = real_events_until_stopped(&mut fixture, case.reason()).await;
                    let errors: Vec<_> = events
                        .iter()
                        .enumerate()
                        .filter(|(_, event)| matches!(event, Event::PromptRuntimeError { .. }))
                        .collect();
                    let quotas: Vec<_> = events
                        .iter()
                        .enumerate()
                        .filter(|(_, event)| matches!(event, Event::RateLimit { .. }))
                        .collect();
                    match case {
                        RealAdoptedOutcome::Completed => {
                            assert!(errors.is_empty() && quotas.is_empty())
                        }
                        RealAdoptedOutcome::Error => {
                            assert_eq!(errors.len(), 1);
                            assert!(quotas.is_empty());
                            assert!(errors[0].0 < events.len() - 1);
                            match errors[0].1 {
                                Event::PromptRuntimeError {
                                    message,
                                    evidence: Some(evidence),
                                    ..
                                } => {
                                    assert_eq!(message, "original request failed");
                                    assert_eq!(
                                        evidence.source,
                                        crate::session::ErrorEvidenceSource::RunnerCompletion
                                    );
                                    assert_eq!(
                                        evidence.runtime,
                                        crate::session::RuntimeEvidence::Unknown
                                    );
                                    assert_eq!(evidence.origin, None);
                                    assert_eq!(evidence.retryable, None);
                                }
                                event => panic!("expected bounded original diagnostic: {event:?}"),
                            }
                        }
                        RealAdoptedOutcome::Quota => {
                            assert!(errors.is_empty());
                            assert_eq!(quotas.len(), 1);
                            assert!(quotas[0].0 < events.len() - 1);
                            assert!(matches!(quotas[0].1, Event::RateLimit { info }
                            if info.resets_at.is_some_and(|at| at.timestamp() == 4_102_444_800)));
                        }
                    }
                    assert_eq!(terminal.current_turn(), turn);
                    assert!(terminal.claimed());
                    assert!(!in_flight.load(AtomicOrdering::Acquire));
                    assert_one_real_stopped(
                        &fixture,
                        before,
                        &request,
                        &head.scope,
                        head.window_id,
                    );
                    let before_next = fixture.store.highest_seq(&fixture.client.id);
                    let next = fixture
                        .start_prompt("next explicit original after native result")
                        .await;
                    assert_ne!(next["id"], request["id"]);
                    assert_eq!(next["params"]["sessionId"], request["params"]["sessionId"]);
                    let (next_scope, next_window) = real_pending_scope(&control, &next).await;
                    assert_ne!(next_scope.correlation(), head.scope.correlation());
                    assert_ne!(next_window, head.window_id);
                    fixture.complete(&next, "end_turn").await;
                    let events = real_events_until_stopped(&mut fixture, "prompt_complete").await;
                    assert!(!events.iter().any(|event| matches!(
                        event,
                        Event::PromptRuntimeError { .. }
                            | Event::RateLimit { .. }
                            | Event::SessionContextReset { .. }
                    )));
                    assert_one_real_stopped(&fixture, before_next, &next, &next_scope, next_window);
                    let methods = fixture.native.requests();
                    assert_eq!(
                        methods
                            .iter()
                            .filter(|method| *method == "session/new")
                            .count(),
                        1
                    );
                    assert_eq!(
                        methods
                            .iter()
                            .filter(|method| *method == "session/prompt")
                            .count(),
                        2
                    );
                    assert!(!methods.iter().any(|method| matches!(
                        method.as_str(),
                        "session/load" | "session/fork" | "session/cancel"
                    )));
                    fixture.client.shutdown().await.unwrap();
                }
            })
        })
        .await;
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn managed_old_completion_markers_cannot_consume_the_next_local_outcome() {
        use crate::acp::event_sender::{EventScope, SdkEvent};
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            for adopt_original in [false, true] {
                let mut fixture = ManagedControlFixture::new(scope).await;
                let original = fixture.start_prompt("first real original").await;
                let mut fixture = if adopt_original { fixture.adopt_in_flight(scope).await } else { fixture };
                let actual = fixture.client.actual();
                let control = actual.test_control();
                let state = actual.test_connection_state();
                let terminal = state.test_terminal.get().unwrap().clone();
                let in_flight = state.test_in_flight.get().unwrap().clone();
                let mut observed = observe_real_relay(&control);
                fixture.complete(&original, "end_turn").await;
                let old_marker = real_completion_marker(&mut observed, TestRelayStage::Relayed, &original).await;
                assert_eq!(old_marker.adopted.is_some(), adopt_original);
                real_events_until_stopped(&mut fixture, "prompt_complete").await;
                let before = fixture.store.highest_seq(&fixture.client.id);
                let next = fixture.start_prompt("second real original").await;
                let (next_scope, next_window) = real_pending_scope(&control, &next).await;
                let next_turn = terminal.current_turn();
                assert!(!terminal.claimed());
                assert!(in_flight.load(AtomicOrdering::Acquire));
                let shim = state.test_shim.get().unwrap().clone();
                let write = shim.lock().await;
                fixture.complete(&next, "end_turn").await;
                let next_marker = real_completion_marker(&mut observed, TestRelayStage::Prepared, &next).await;
                assert_real_settled(&control, &next, &next_scope, next_window);
                control.deliver_prompt_completion(old_marker.clone());
                assert_real_settled(&control, &next, &next_scope, next_window);
                if let Some(adopted) = old_marker.adopted {
                    let evidence = adopted.evidence.as_ref().expect("actual original evidence");
                    assert_eq!(Some(evidence.prompt_req_id), original["id"].as_i64());
                    let old_correlation = evidence.scope.correlation();
                    let old_window = evidence.window_id;
                    let (events, mut received) = mpsc::channel::<SdkEvent>(4);
                    let events: crate::acp::event_sender::EventSender = events.into();
                    let publication = publish_adopted_completion(
                        *adopted, &events, &control, &in_flight, &terminal, || None,
                    ).await;
                    assert!(matches!(publication, AdoptedCompletionPublication::Historical));
                    let data = match received.recv().await.unwrap() {
                        SdkEvent::Data(data) => data,
                        other => panic!("pure history consumer expects Data, got {other:?}"),
                    };
                    assert!(matches!(data.event.as_ref(), Event::Stopped { reason } if reason == "prompt_complete"));
                    assert_eq!(data.metadata.scope, EventScope::HistoryOnly);
                    assert_eq!(data.metadata.native_prompt_id, original["id"].as_i64());
                    assert_eq!(data.metadata.operation.map(|operation| operation.correlation), old_correlation);
                    assert_eq!(data.metadata.prompt_observation, Some(
                        crate::acp::protocol::AcpPromptObservation::Written { window_id: old_window }
                    ));
                    assert!(matches!(received.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
                } else {
                    assert_ne!(old_marker.prompt_req_id, next_marker.prompt_req_id);
                    assert_ne!(old_marker.correlation, next_marker.correlation);
                }
                assert_real_settled(&control, &next, &next_scope, next_window);
                assert_eq!(terminal.current_turn(), next_turn);
                assert!(!terminal.claimed());
                assert!(in_flight.load(AtomicOrdering::Acquire));
                drop(write);
                real_completion_marker(&mut observed, TestRelayStage::Relayed, &next).await;
                real_events_until_stopped(&mut fixture, "prompt_complete").await;
                assert_one_real_stopped(&fixture, before, &next, &next_scope, next_window);
                assert_eq!(fixture.native.requests().iter().filter(|method| *method == "session/prompt").count(), 2);
                fixture.client.shutdown().await.unwrap();
            }
        })).await;
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn managed_duplicate_local_prompt_preserves_the_real_first_waiter() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                let mut fixture = ManagedControlFixture::new(scope).await;
                let before = fixture.store.highest_seq(&fixture.client.id);
                let request = fixture
                    .start_prompt("only this original may reach native stdin")
                    .await;
                let actual = fixture.client.actual();
                let control = actual.test_control();
                let (pending_scope, window) = real_pending_scope(&control, &request).await;
                let duplicate = control
                    .prompt(request["params"].clone(), pending_scope.clone())
                    .await;
                assert!(matches!(duplicate, Err(AcpError::Protocol(_))));
                let (still_pending, still_window) = real_pending_scope(&control, &request).await;
                assert_eq!(still_pending, pending_scope);
                assert_eq!(still_window, window);
                assert_eq!(
                    fixture
                        .native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/prompt")
                        .count(),
                    1
                );
                fixture.complete(&request, "cancelled").await;
                real_events_until_stopped(&mut fixture, "cancelled").await;
                assert_one_real_stopped(&fixture, before, &request, &pending_scope, window);
                assert_eq!(
                    fixture
                        .native
                        .requests()
                        .iter()
                        .filter(|method| *method == "session/prompt")
                        .count(),
                    1
                );
                assert!(!fixture
                    .native
                    .requests()
                    .iter()
                    .any(|method| method == "session/cancel"));
                fixture.client.shutdown().await.unwrap();
            })
        })
        .await;
    }
    #[derive(Clone, Copy, Debug)]
    enum RealRelayTiming {
        MarkerWriteFails,
        ControlEofAfterMarker,
        LaterWriteFails,
        ControlEofBeforeResult,
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn managed_prompt_relays_preserve_results_and_fence_eof() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        use tokio::io::AsyncWriteExt as _;
        const NAME: &str =
            "acp::acp_client::control::tests::managed_prompt_relays_preserve_results_and_fence_eof";
        if crate::process::runner::test_support::managed_runner::child_if_requested(NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
            for timing in [RealRelayTiming::MarkerWriteFails, RealRelayTiming::ControlEofAfterMarker,
                RealRelayTiming::LaterWriteFails, RealRelayTiming::ControlEofBeforeResult]
            {
                let inspection = scope.inspect_next_runner(NAME);
                let mut fixture = ManagedControlFixture::new(scope).await;
                let before = fixture.store.highest_seq(&fixture.client.id);
                let request = fixture.start_prompt("real local original held for relay timing").await;
                let actual = fixture.client.actual();
                let control = actual.test_control();
                let state = actual.test_connection_state();
                let (pending_scope, window) = real_pending_scope(&control, &request).await;
                assert_ne!(control.peer_identity().pid, std::process::id());
                let mut observed = observe_real_relay(&control);
                let shim = state.test_shim.get().unwrap().clone();
                let mut update_release = None;
                if matches!(timing, RealRelayTiming::ControlEofAfterMarker | RealRelayTiming::LaterWriteFails) {
                    let (gate, release, entered) = scope.gate();
                    actual.gate_next_update(gate);
                    fixture.native.send(serde_json::json!({
                        "jsonrpc":"2.0", "method":"session/update", "params":{
                            "sessionId":request["params"]["sessionId"], "update":{
                                "sessionUpdate":"agent_message_chunk",
                                "content":{"type":"text", "text":"real update before the original result"}
                            }
                        }
                    })).await;
                    real_control_deadline(entered).await.unwrap();
                    update_release = Some(release);
                }
                match timing {
                    RealRelayTiming::MarkerWriteFails => {
                        let mut write = shim.lock().await;
                        fixture.complete(&request, "cancelled").await;
                        real_completion_marker(&mut observed, TestRelayStage::Prepared, &request).await;
                        assert_real_settled(&control, &request, &pending_scope, window);
                        write.shutdown().await.unwrap();
                        drop(write);
                        real_completion_marker(&mut observed, TestRelayStage::Failed, &request).await;
                    }
                    RealRelayTiming::ControlEofAfterMarker => {
                        fixture.complete(&request, "cancelled").await;
                        real_completion_marker(&mut observed, TestRelayStage::Relayed, &request).await;
                        assert_real_settled(&control, &request, &pending_scope, window);
                        inspection.query("close-control").await;
                        real_control_deadline(async {
                            loop {
                                if control.handshake_rx.lock().await.is_closed() { break; }
                                tokio::task::yield_now().await;
                            }
                        }).await;
                    }
                    RealRelayTiming::LaterWriteFails => {
                        fixture.complete(&request, "cancelled").await;
                        real_completion_marker(&mut observed, TestRelayStage::Relayed, &request).await;
                        assert_real_settled(&control, &request, &pending_scope, window);
                        let mut write = shim.lock().await;
                        fixture.native.send(serde_json::json!({
                            "jsonrpc":"2.0", "method":"session/update", "params":{
                                "sessionId":request["params"]["sessionId"], "update":{
                                    "sessionUpdate":"agent_message_chunk",
                                    "content":{"type":"text", "text":"the later relay whose real write must fail"}
                                }
                            }
                        })).await;
                        let later = real_relay_observation(&mut observed, TestRelayStage::Prepared, "session/update").await;
                        assert_eq!(later["params"]["update"]["content"]["text"],
                            "the later relay whose real write must fail");
                        write.shutdown().await.unwrap();
                        drop(write);
                        real_relay_observation(&mut observed, TestRelayStage::Failed, "session/update").await;
                    }
                    RealRelayTiming::ControlEofBeforeResult => { inspection.query("close-control").await; }
                }
                // A real loss may cancel the callback receiver before release.
                if let Some(release) = update_release { let _ = release.release(); }
                let events = real_control_deadline(async {
                    let mut events = Vec::new();
                    loop {
                        let event = fixture.next_event().await;
                        assert!(!matches!(&event, Event::AgentStartupError { .. }), "{event:?}");
                        let unavailable = matches!(&event, Event::AcpConnectionUnavailable { .. });
                        events.push(event);
                        if unavailable { break events; }
                    }
                }).await;
                let loss = state.loss().expect("a real relay/control endpoint ended");
                if matches!(timing, RealRelayTiming::ControlEofAfterMarker | RealRelayTiming::ControlEofBeforeResult) {
                    assert_eq!(loss.source, crate::acp::state::AcpConnectionSource::Control);
                    assert_eq!(loss.cause, crate::acp::state::AcpConnectionCause::IncomingEnded);
                } else {
                    // The actual shim writer and SDK reader can race to observe
                    // this physical closure. Failed above proves the write I/O;
                    // the first real typed cause must remain sticky below.
                    assert!(matches!((loss.source, loss.cause),
                        (crate::acp::state::AcpConnectionSource::Control,
                         crate::acp::state::AcpConnectionCause::ForwardPumpEnded)
                        | (crate::acp::state::AcpConnectionSource::Sdk,
                           crate::acp::state::AcpConnectionCause::IncomingEnded)
                    ), "unexpected observed relay loss: {loss:?}");
                }
                assert!(!control.peer_process.exited().unwrap(), "control/shim EOF is not native owner death");
                let fatal = events.iter().position(|event| matches!(event,
                    Event::AcpConnectionUnavailable { .. }
                )).unwrap();
                assert_eq!(events.iter().filter(|event| matches!(event,
                    Event::AcpConnectionUnavailable { .. }
                )).count(), 1);
                if matches!(timing, RealRelayTiming::ControlEofBeforeResult) {
                    assert!(!events.iter().any(|event| matches!(event,
                        Event::Stopped { reason } if reason == "prompt_complete" || reason == "cancelled"
                    )));
                    let stored = fixture.store.replay_from(&fixture.client.id, before).unwrap();
                    assert!(!stored.iter().any(|stored|
                        matches!(stored.event.as_ref(), Event::Stopped { .. })
                            && stored.metadata.native_prompt_id == request["id"].as_i64()
                            && stored.metadata.operation.is_some_and(|operation| operation.outcome.is_some())
                    ), "an unanswered original has no native result evidence");
                } else {
                    let stopped = events.iter().position(|event| matches!(event,
                        Event::Stopped { reason } if reason == "cancelled"
                    )).expect("the actual original stop reason survives the failed route");
                    assert!(stopped < fatal);
                    assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { .. })).count(), 1);
                    assert!(!events.iter().any(|event| matches!(event,
                        Event::PromptRuntimeError { .. } | Event::RateLimit { .. }
                    )));
                    assert_one_real_stopped(&fixture, before, &request, &pending_scope, window);
                }
                let native_requests = fixture.native.requests();
                assert_eq!(native_requests.iter().filter(|method| *method == "session/prompt").count(), 1);
                assert!(matches!(control.cancel(pending_scope.clone()).await,
                    Err(AcpError::ConnectionUnavailable(observation)) if observation == loss));
                let refused = control.prompt(request["params"].clone(), pending_scope).await;
                assert!(matches!(refused,
                    Err(AcpError::ConnectionUnavailable(observation)) if observation == loss));
                assert_eq!(fixture.native.requests(), native_requests);
                assert_eq!(state.loss(), Some(loss));
                fixture.client.shutdown().await.unwrap();
                assert_eq!(fixture.native.requests(), native_requests,
                    "physical cleanup cannot send native input after observed loss");
                assert_eq!(state.loss(), Some(loss));
            }
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn deferred_original_error_pair_cannot_settle_the_next_real_prompt() {
        use crate::acp::event_sender::{EventScope, SdkEvent};
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let mut fixture = ManagedControlFixture::new(scope).await;
            let original = fixture.start_prompt("actual original before adoption").await;
            let mut fixture = fixture.adopt_in_flight(scope).await;
            let actual = fixture.client.actual();
            let control = actual.test_control();
            let state = actual.test_connection_state();
            let terminal = state.test_terminal.get().unwrap().clone();
            let in_flight = state.test_in_flight.get().unwrap().clone();
            let mut observed = observe_real_relay(&control);
            RealAdoptedOutcome::Error.reply(&fixture, &original).await;
            let marker = real_completion_marker(&mut observed, TestRelayStage::Relayed, &original).await;
            real_events_until_stopped(&mut fixture, "prompt_failed").await;
            let adopted = marker.adopted.expect("real adopted Native error marker");
            let evidence = adopted.evidence.as_ref().expect("real original result evidence");
            let old_correlation = evidence.scope.correlation();
            let old_window = evidence.window_id;
            let (events, mut received) = mpsc::channel::<SdkEvent>(2);
            let events: crate::acp::event_sender::EventSender = events.into();
            events.send(Event::AvailableCommandsUpdated { commands: vec![] }).await.unwrap();
            let publication = publish_adopted_completion(
                *adopted, &events, &control, &in_flight, &terminal, || None,
            );
            tokio::pin!(publication);
            assert!(futures_util::poll!(publication.as_mut()).is_pending());
            let next = fixture.start_prompt("distinct actual input while historical pair waits").await;
            let (next_scope, next_window) = real_pending_scope(&control, &next).await;
            let next_turn = terminal.current_turn();
            assert!(!terminal.claimed());
            assert!(in_flight.load(AtomicOrdering::Acquire));
            assert!(matches!(received.recv().await, Some(SdkEvent::Data(data))
                if matches!(data.event.as_ref(), Event::AvailableCommandsUpdated { .. })));
            assert!(matches!(real_control_deadline(publication).await,
                AdoptedCompletionPublication::Historical));
            for diagnostic in [true, false] {
                let data = match real_control_deadline(received.recv()).await.unwrap() {
                    SdkEvent::Data(data) => data,
                    other => panic!("expected historical original component: {other:?}"),
                };
                assert_eq!(data.metadata.scope, EventScope::HistoryOnly);
                assert_eq!(data.metadata.native_prompt_id, original["id"].as_i64());
                assert_eq!(data.metadata.operation.map(|operation| operation.correlation), old_correlation);
                assert_eq!(data.metadata.prompt_observation, Some(
                    crate::acp::protocol::AcpPromptObservation::Written { window_id: old_window }));
                if diagnostic {
                    assert!(matches!(data.event.as_ref(), Event::PromptRuntimeError { .. }));
                } else {
                    assert!(matches!(data.event.as_ref(), Event::Stopped { reason } if reason == "prompt_failed"));
                }
            }
            assert!(matches!(received.try_recv(), Err(mpsc::error::TryRecvError::Empty)));
            assert_eq!(real_pending_scope(&control, &next).await, (next_scope, next_window));
            assert_eq!(terminal.current_turn(), next_turn);
            assert!(!terminal.claimed());
            assert!(in_flight.load(AtomicOrdering::Acquire));
            fixture.complete(&next, "end_turn").await;
            real_events_until_stopped(&mut fixture, "prompt_complete").await;
            assert_eq!(fixture.native.requests().iter().filter(|method| *method == "session/prompt").count(), 2);
            fixture.client.shutdown().await.unwrap();
        })).await;
    }

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
            prompt_head: None,
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

    async fn connect(
        control: &std::path::Path,
        session: &str,
        terminal: Arc<TerminalClaim>,
        in_flight: bool,
    ) -> Result<(Arc<DaemonControlClient>, tokio::io::DuplexStream), AcpError> {
        let in_flight = Arc::new(AtomicBool::new(in_flight));
        connect_runner_control(
            control,
            session.into(),
            terminal,
            in_flight,
            fixture_owner(),
            Arc::new(crate::acp::event_sender::ProjectionState::read_only(None)),
        )
        .await
    }

    #[tokio::test]
    async fn oversized_reverse_reply_becomes_error_without_poisoning_connection() {
        use tokio::io::AsyncWriteExt;
        let tmp = tempfile::Builder::new()
            .prefix("aoe-ctl-")
            .tempdir_in("/tmp")
            .unwrap();
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
                    evidence: control_protocol::NativeNotifyEvidence::default(),
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
    #[serial_test::serial]
    async fn attached_prompt_errors_never_rebase_a_successfully_resumed_session() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
                let message = "Unsupported ACP session";
                let directory = scope.tempdir();
                let mut prompts = 0;
                let (config, native) = scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
                    let id = request.get("id")?;
                    Some(match request["method"].as_str().unwrap() {
                        "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                        "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"sid-stored"}}),
                        "session/prompt" => {
                            assert_eq!(request["params"]["sessionId"], "sid-stored");
                            prompts += 1;
                            if prompts == 1 {
                                serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                                    "code":-32603,"message":message,
                                    "data":{"reason":"incoming_transport_closed","privateTokenMarker":"must-not-leak"}
                                }})
                            } else {
                                serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}})
                            }
                        }
                        method => panic!("unexpected native operation after genuine retained New: {method}"),
                    })
                });
                let original = scope.spawn(config, "resumed-request-error").await.unwrap();
                original.ready().await;
                let mut client = scope.adopt(original, false).await;
                client.ready().await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    for (text, expected_reason) in [
                        ("continue", "prompt_failed"),
                        ("explicit next request", "prompt_complete"),
                    ] {
                        client.send_prompt(text, &[]).await.unwrap();
                        let mut diagnostics = 0;
                        loop {
                            let event = client.next_event().await.expect("same real SDK connection remains live");
                            assert!(!serde_json::to_string(&event).unwrap().contains("privateTokenMarker"));
                            match event {
                                Event::SessionContextReset { .. } => panic!("ordinary request prose must not rebase"),
                                Event::AgentStartupError { message, .. } => panic!("{message}"),
                                Event::AcpConnectionUnavailable { .. } => panic!("peer data is not an EOF observation"),
                                Event::PromptRuntimeError { message: error, .. } => {
                                    assert_eq!(expected_reason, "prompt_failed");
                                    assert_eq!(error, message);
                                    diagnostics += 1;
                                }
                                Event::Stopped { reason } => {
                                    assert_eq!(reason, expected_reason);
                                    assert_eq!(diagnostics, usize::from(expected_reason == "prompt_failed"));
                                    break;
                                }
                                _ => {}
                            }
                        }
                    }
                }).await.expect("same-runtime request recovery completes");
                client.shutdown().await.unwrap();
                let requests = native.requests();
                assert_eq!(requests.iter().filter(|method| *method == "initialize").count(), 1);
                assert_eq!(requests.iter().filter(|method| *method == "session/new").count(), 1);
                assert_eq!(requests.iter().filter(|method| *method == "session/prompt").count(), 2);
                assert!(!requests.iter().any(|method| matches!(method.as_str(), "session/load" | "session/fork")));
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_identity_reattach_uses_runner_id_and_drains_adopted_updates() {
        use crate::acp::acp_client::SdkTestOptions;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let directory = scope.tempdir();
            let own_file = directory.path().join("own");
            let foreign_file = directory.path().join("foreign");
            let (config, mut native) = scope.native_peer(directory.path().into(), "codex", "codex", |request| {
                let id = request.get("id")?;
                Some(match request["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                    "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"actual"}}),
                    other => panic!("adoption must not issue another native session operation: {other}"),
                })
            });
            let original = scope.spawn(config, "native-resume").await.unwrap();
            original.ready().await;
            let storage = crate::session::Storage::open_unwatched("test").unwrap();
            let before = storage.load().unwrap().into_iter().find(|row| row.id == original.id).unwrap();
            let (fence, release, entered) = scope.gate();
            let (observed, mut control) = mpsc::unbounded_channel();
            scope.configure_sdk(SdkTestOptions { bootstrap_fence: Some(fence), control_output: Some(observed), ..Default::default() });
            let mut client = scope.adopt(original, false).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                entered.await.unwrap();
                loop {
                    match control.recv().await.unwrap() {
                        ControlBody::Initialize { .. } => {}
                        ControlBody::ResumeSession => break,
                        other => panic!("unexpected actual adoption request: {other:?}"),
                    }
                }
                let update = |text: &str| serde_json::json!({
                    "sessionUpdate":"agent_message_chunk",
                    "content":{"type":"text","text":text}
                });
                for index in 0..200 {
                    native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                        "sessionId":"actual","update":update(&format!("own-{index}"))
                    }})).await;
                }
                native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":"stale","update":update("FOREIGN")
                }})).await;
                for params in [
                    serde_json::Value::Null,
                    serde_json::json!(7),
                    serde_json::json!([]),
                    serde_json::json!({"update":update("INVALID")}),
                    serde_json::json!({"notification":{"sessionId":"actual","update":update("INVALID")},"__aoe_control_frame_bytes":0}),
                    serde_json::json!(["actual", update("INVALID"), null, 0]),
                ] {
                    native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":params})).await;
                }
                let deep = (0..124).fold(serde_json::Value::Null, |value, _| serde_json::Value::Array(vec![value]));
                for params in [
                    serde_json::json!(["actual",update("own-200"),{"deep":deep}]),
                    serde_json::json!({"sessionId":"actual","update":update("own-201"),"__aoe_control_frame_bytes":{"untrusted":true}}),
                    serde_json::json!({"sessionId":"actual","update":update("own-202"),"__aoe_control_frame_bytes":u64::MAX}),
                    serde_json::json!(["actual", update("own-203")]),
                ] {
                    native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":params})).await;
                }
                native.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"fs/write_text_file","params":{
                    "sessionId":"stale","path":foreign_file,"content":"owned"
                }})).await;
                let denied = native.reply().await;
                assert_eq!(denied["id"], 2);
                assert_eq!(denied["error"]["code"], -32602);
                assert!(release.release());
                client.ready().await;
                native.send(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"fs/write_text_file","params":{
                    "sessionId":"actual","path":own_file,"content":"owned"
                }})).await;
                let accepted = native.reply().await;
                assert_eq!(accepted["id"], 1);
                assert!(accepted.get("error").is_none(), "{accepted}");
                let mut chunks = Vec::new();
                let mut resets = 0;
                let mut assigned = false;
                while chunks.len() < 205 || !assigned {
                    match client.next_event().await.unwrap() {
                        Event::AgentMessageChunk { text, .. } => chunks.push(text),
                        Event::SessionContextReset { .. } => resets += 1,
                        Event::AcpSessionAssigned { .. } => assigned = true,
                        Event::AgentStartupError { message, .. } => panic!("{message}"),
                        Event::AcpConnectionUnavailable { .. } => panic!("actual native owner remains live"),
                        _ => {}
                    }
                }
                assert_eq!(resets, 0, "certified adoption cannot replace a different durable selection");
                assert_eq!(chunks.remove(200), "FOREIGN");
                assert_eq!(chunks, (0..204).map(|index| format!("own-{index}")).collect::<Vec<_>>());
                let events = client.app_state().acp_event_store.replay_from(&client.id, 0).unwrap();
                let foreign = events.iter().find(|event| matches!(&*event.event,
                    Event::AgentMessageChunk { text, .. } if text == "FOREIGN"
                )).expect("foreign neutral output remains visible as history");
                assert_eq!(foreign.metadata.scope, crate::acp::event_sender::EventScope::HistoryOnly);
                assert_eq!(foreign.metadata.reported_session_id.as_ref().map(|id| id.0.as_ref()), Some("stale"));
                assert!(foreign.metadata.operation.is_none());
                assert_eq!(std::fs::read_to_string(&own_file).unwrap(), "owned");
                assert!(!foreign_file.exists());
                let after = storage.load().unwrap().into_iter().find(|row| row.id == client.id).unwrap();
                assert_eq!(after.acp_session_id, before.acp_session_id);
                assert_eq!(after.resume_binding, before.resume_binding);
                assert_eq!(after.acp_establishment, before.acp_establishment);
                client.shutdown().await.unwrap();
            }).await.expect("actual adopted callbacks and complete identity backlog finish");
        })).await;
    }
    #[tokio::test]
    #[serial_test::serial]
    async fn same_id_reattach_announces_no_reset() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let directory = scope.tempdir();
            let (config, native) = scope.native_peer(directory.path().into(), "codex", "codex", |request| {
                let id = request.get("id")?;
                Some(match request["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                    "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"actual"}}),
                    other => panic!("no new native operation on same-owner adoption: {other}"),
                })
            });
            let original = scope.spawn(config, "same-id").await.unwrap();
            original.ready().await;
            let mut client = scope.adopt(original, false).await;
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match client.next_event().await.unwrap() {
                        Event::SessionContextReset { .. } => panic!("same qualified choice must not reset"),
                        Event::AcpSessionAssigned { .. } => break,
                        Event::AgentStartupError { message, .. } => panic!("{message}"),
                        Event::AcpConnectionUnavailable { .. } => panic!("same physical native owner remains live"),
                        _ => {}
                    }
                }
                client.shutdown().await.unwrap();
            }).await.expect("real same-owner adoption assigns its certified session");
            assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 1);
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_identity_reattach_preserves_a_full_wire_byte_backlog() {
        use crate::acp::acp_client::SdkTestOptions;
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let directory = scope.tempdir();
            let own_file = directory.path().join("own");
            let (config, mut native) = scope.native_peer(directory.path().into(), "codex", "codex", |request| {
                let id = request.get("id")?;
                Some(match request["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                    "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s"}}),
                    other => panic!("no native RPC on read-only adoption: {other}"),
                })
            });
            let original = scope.spawn(config, "byte-backlog").await.unwrap();
            original.ready().await;
            let (fence, release, entered) = scope.gate();
            let (native_seen, received_native) = oneshot::channel();
            scope.configure_sdk(SdkTestOptions {
                bootstrap_fence: Some(fence), native_session_seen: Some(native_seen),
                ..Default::default()
            });
            let mut client = scope.adopt(original, false).await;
            tokio::time::timeout(Duration::from_secs(120), async {
                entered.await.unwrap();
                received_native.await.unwrap();
                let ingress = Arc::clone(&client.actual().test_control().ingress);
                let state = client.actual().test_connection_state();
                for (index, id) in ["a", "b", "c"].into_iter().enumerate() {
                    // Typed f64 expansion exceeds the budget, but genuine wire
                    // provenance and all three native updates still fit.
                    let body = ControlBody::Notify { method: "session/update".into(), params: serde_json::json!({
                        "sessionId":"s",
                        "update":{"sessionUpdate":"tool_call","toolCallId":id,"title":"t",
                            "content":vec![serde_json::json!({"type":"content","content":{
                                "type":"text","text":"x","annotations":{"priority":1}}});20_000]},
                        "_meta":{"pad":""}
                    }), replay: None, evidence: crate::acp::control_protocol::NativeNotifyEvidence::default() };
                    let frame = control_protocol::encode_frame(&body).unwrap();
                    let padding = (control_protocol::MAX_CONTROL_QUEUE_BYTES - 65_536) / 3 - frame.len();
                    drop(frame);
                    let ControlBody::Notify { mut params, .. } = body else { unreachable!() };
                    params["_meta"]["pad"] = serde_json::Value::String("x".repeat(padding));
                    native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":params})).await;
                    loop {
                        let pending = ingress.test_pending_footprint(false);
                        let loss = state.loss();
                        assert!(loss.is_none(), "native byte admission {index}: loss={loss:?}; pending={pending:?}");
                        let (failed, updates, wire_bytes, _) = pending.expect("actual bootstrap still owns pending replay");
                        assert!(!failed, "native byte admission {index}: updates={updates}, wire_bytes={wire_bytes}");
                        if updates == index + 1 {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                }
                native.send(serde_json::json!({"jsonrpc":"2.0","id":1,"method":"fs/read_text_file","params":{
                    "sessionId":"unknown","path":own_file
                }})).await;
                let denied = native.reply().await;
                assert_eq!(denied["id"], 1);
                assert_eq!(denied["error"]["code"], -32602, "{denied}");
                let (failed, updates, wire_bytes, typed_bytes) = ingress
                    .test_pending_footprint(true).expect("bootstrap remains fenced");
                assert!(!failed);
                assert_eq!(updates, 3);
                assert!(wire_bytes <= control_protocol::MAX_CONTROL_QUEUE_BYTES);
                assert!(typed_bytes > control_protocol::MAX_CONTROL_QUEUE_BYTES);
                while let Ok(event) = client.try_recv() {
                    assert!(!matches!(event, Event::AcpSessionAssigned { .. } | Event::ToolCallStarted { .. }),
                        "publication crossed the actual SDK bootstrap fence: {event:?}");
                }
                assert!(release.release());
                client.ready().await;
                native.send(serde_json::json!({"jsonrpc":"2.0","id":2,"method":"fs/write_text_file","params":{
                    "sessionId":"s","path":own_file,"content":"owned"
                }})).await;
                let accepted = native.reply().await;
                assert_eq!(accepted["id"], 2);
                assert!(accepted.get("error").is_none(), "{accepted}");
                let mut calls = Vec::new();
                while calls.len() < 3 {
                    match client.next_event().await.unwrap() {
                        Event::ToolCallStarted { tool_call } => calls.push(tool_call.id),
                        Event::AgentStartupError { message, .. } => panic!("{message}"),
                        Event::AcpConnectionUnavailable { .. } => panic!("producer-admitted byte backlog must remain live"),
                        _ => {}
                    }
                }
                assert_eq!(calls, ["a", "b", "c"]);
                assert_eq!(std::fs::read_to_string(&own_file).unwrap(), "owned");
                client.shutdown().await.unwrap();
            }).await.expect("full real wire-byte backlog survives actual SDK admission");
        })).await;
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
        let tmp = tempfile::Builder::new()
            .prefix("aoe-ctl-")
            .tempdir_in("/tmp")
            .unwrap();
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

    /// Actual SDK Load replay and prompt completion wait for preceding update application.
    #[tokio::test]
    #[serial_test::serial]
    async fn barriers_wait_for_updates_applied_by_the_crate() {
        use crate::acp::acp_client::SdkTestOptions;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use futures_util::FutureExt as _;
        with_managed(|scope| Box::pin(async move {
            let directory = scope.tempdir();
            let (prime_config, _) = scope.native_peer(directory.path().into(), "codex", "codex", |request| {
                let id = request.get("id")?;
                Some(match request["method"].as_str().unwrap() {
                    "initialize" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}}),
                    "session/new" => serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"s"}}),
                    other => panic!("unexpected original native operation: {other}"),
                })
            });
            let original = scope.spawn(prime_config, "replay-prime").await.unwrap();
            original.ready().await;
            let (load_id, load_requested) = oneshot::channel();
            let (prompt_id, mut prompt_requested) = mpsc::unbounded_channel();
            let mut load_id = Some(load_id);
            let (config, native) = scope.native_peer(directory.path().into(), "codex", "codex", move |request| {
                let id = request.get("id")?;
                match request["method"].as_str().unwrap() {
                    "initialize" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{
                        "protocolVersion":1,"agentCapabilities":{"loadSession":true}
                    }})),
                    "session/load" => {
                        assert_eq!(request["params"]["sessionId"], "s");
                        load_id.take().unwrap().send(id.clone()).unwrap();
                        None
                    }
                    "session/prompt" => {
                        assert_eq!(request["params"]["sessionId"], "s");
                        prompt_id.send(id.clone()).unwrap();
                        None
                    }
                    other => panic!("no implicit New or retry in a real retained Load: {other}"),
                }
            });
            let (update_gate, release_update, entered_update) = scope.gate();
            scope.configure_sdk(SdkTestOptions { update: Some(update_gate), ..Default::default() });
            let mut client = scope.retained_after(original, config, "replayed").await;
            tokio::time::timeout(Duration::from_secs(15), async {
                let id = load_requested.await.unwrap();
                native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":"s","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"replayed"}}
                }})).await;
                native.send(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}})).await;
                entered_update.await.unwrap();
                let control = client.actual().test_control();
                let replayed = control.session_replayed();
                tokio::pin!(replayed);
                assert!(replayed.as_mut().now_or_never().is_none(), "Load cannot count as replayed mid-handler");
                let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
                loop {
                    match tokio::time::timeout_at(deadline, client.next_event()).await {
                        Err(_) => break,
                        Ok(None) => panic!("retained Load closed while its update handler was held"),
                        Ok(Some(event)) => assert!(!matches!(event, Event::AcpSessionAssigned { .. }),
                            "assignment crossed the actual held SDK update handler"),
                    }
                }
                assert!(release_update.release());
                replayed.await;
                client.ready().await;
                loop {
                    match client.next_event().await.expect("actual parent publishes the replay") {
                        Event::AcpSessionAssigned { .. } => break,
                        Event::AgentStartupError { message, .. } => panic!("{message}"),
                        Event::AcpConnectionUnavailable { .. } => panic!("retained Load remains live"),
                        _ => {}
                    }
                }
                for limited in [false, true] {
                    let (update_gate, release_update, entered_update) = scope.gate();
                    client.actual().gate_next_update(update_gate);
                    client.send_prompt("apply usage before settling", &[]).await.unwrap();
                    let id = prompt_requested.recv().await.unwrap();
                    for (session, used, resets_at) in [("s", 100, 4_102_444_800_i64), ("foreign", 999, 4_102_444_900_i64)] {
                        native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                            "sessionId":session,"update":{
                                "sessionUpdate":"usage_update","used":used,"size":200,
                                "_meta":{"_claude/rateLimit":{
                                    "status":"rejected","rateLimitType":"five_hour","resetsAt":resets_at
                                }}
                            }
                        }})).await;
                    }
                    let outcome = if limited {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-32000,"message":"actual native quota refusal","data":{"errorKind":"rate_limit"}
                        }})
                    } else {
                        serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}})
                    };
                    native.send(outcome).await;
                    entered_update.await.unwrap();
                    while control.settled.lock().unwrap().is_none() {
                        while let Ok(event) = client.try_recv() {
                            assert!(!matches!(event, Event::Stopped { .. } | Event::RateLimit { .. }),
                                "prompt settled before its actual SDK update handler: {event:?}");
                        }
                        tokio::task::yield_now().await;
                    }
                    let deadline = tokio::time::Instant::now() + Duration::from_millis(100);
                    loop {
                        match tokio::time::timeout_at(deadline, client.next_event()).await {
                            Err(_) => break,
                            Ok(None) => panic!("original native request remains live until update application"),
                            Ok(Some(event)) => assert!(!matches!(event, Event::Stopped { .. } | Event::RateLimit { .. }),
                                "native outcome crossed the held SDK update handler: {event:?}"),
                        }
                    }
                    assert!(release_update.release());
                    let mut events = Vec::new();
                    loop {
                        let event = client.next_event().await.unwrap();
                        let stopped = matches!(event, Event::Stopped { .. });
                        events.push(event);
                        if stopped { break; }
                    }
                    let usage = events.iter().position(|event| matches!(event,
                        Event::UsageUpdated { usage } if usage.used == 100)).unwrap();
                    let stopped = events.iter().position(|event| matches!(event, Event::Stopped { .. })).unwrap();
                    assert!(usage < stopped);
                    let quotas: Vec<_> = events.iter().enumerate().filter(|(_, event)| matches!(event, Event::RateLimit { .. })).collect();
                    assert_eq!(quotas.len(), usize::from(limited), "usage metadata cannot invent a Native quota outcome");
                    if limited {
                        assert!(usage < quotas[0].0 && quotas[0].0 < stopped);
                        assert!(matches!(quotas[0].1, Event::RateLimit { info }
                            if info.resets_at.is_some_and(|at| at.timestamp() == 4_102_444_800)));
                    }
                    let expected = if limited { "rate_limited" } else { "prompt_complete" };
                    assert!(matches!(&events[stopped], Event::Stopped { reason } if reason == expected));
                }
                assert_eq!(native.requests().iter().filter(|method| *method == "session/load").count(), 1);
                assert_eq!(native.requests().iter().filter(|method| *method == "session/prompt").count(), 2);
                assert!(!native.requests().iter().any(|method| method == "session/new"));
                client.shutdown().await.unwrap();
            }).await.expect("actual SDK replay and prompt barriers settle in order");
        })).await;
    }
    async fn send_adopted_backlog(
        native: &crate::acp::supervisor::test_support::managed::NativePeer,
        prompt_id: &serde_json::Value,
    ) {
        for index in 0..12 {
            native
                .send(
                    serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                        "sessionId":"sid-adopted","update":{
                            "sessionUpdate":"agent_message_chunk",
                            "content":{"type":"text","text":format!("prior output {index}\n")}
                        }
                    }}),
                )
                .await;
        }
        native
            .send(serde_json::json!({"jsonrpc":"2.0","id":prompt_id,"error":{
                "code":-32603,"message":"adopted request failed",
                "data":{"privateTokenMarker":"must-not-leak"}
            }}))
            .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn fresh_initialize_refusal_never_assigns_or_applies() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| {
            Box::pin(async move {
                // A genuine retained owner cannot change its cached successful
                // initialize into failure. Keep Fresh refusal separate from adoption.
                let directory = scope.tempdir();
                let (config, native) =
                    scope.native_peer(directory.path().into(), "gemini", "gemini", |request| {
                        let id = request.get("id")?;
                        assert_eq!(request["method"], "initialize");
                        Some(serde_json::json!({"jsonrpc":"2.0","id":id,"error":{
                            "code":-32603,"message":"initialize failed",
                            "data":{"privateTokenMarker":"must-not-leak"}
                        }}))
                    });
                let (id, mut observed) = scope.create_observed(config, "initialize-refused").await;
                tokio::time::timeout(Duration::from_secs(10), async {
                    loop {
                        let frame = observed
                            .recv()
                            .await
                            .expect("pre-factory observer must not lag");
                        if frame.session_id != id {
                            continue;
                        }
                        assert!(!serde_json::to_string(&frame.event)
                            .unwrap()
                            .contains("privateTokenMarker"));
                        assert!(!matches!(&*frame.event, Event::AcpSessionAssigned { .. }));
                        if matches!(&*frame.event, Event::AgentStartupError { .. }) {
                            break;
                        }
                    }
                })
                .await
                .expect("real native initialize refusal reaches its actual creator");
                assert_eq!(
                    native
                        .requests()
                        .iter()
                        .filter(|method| *method == "initialize")
                        .count(),
                    1
                );
                assert!(!native
                    .requests()
                    .iter()
                    .any(|method| method == "session/new"));
                let rows = crate::session::Storage::open_unwatched("test")
                    .unwrap()
                    .load()
                    .unwrap();
                assert!(rows.iter().filter(|row| row.id == id).all(|row| !matches!(
                    row.acp_establishment,
                    Some(crate::acp::establishment::AcpEstablishment::Applied { .. })
                )));
            })
        })
        .await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn adopted_failure_barrier_orders_buffered_notifications_and_bootstrap_commit() {
        use crate::acp::acp_client::SdkTestOptions;
        use crate::acp::supervisor::test_support::managed::with_managed;
        use crate::process::runner::test_support::managed_runner::child_if_requested;
        use futures_util::FutureExt as _;
        const TEST_NAME: &str = "acp::acp_client::control::tests::adopted_failure_barrier_orders_buffered_notifications_and_bootstrap_commit";
        if child_if_requested(TEST_NAME).await {
            return;
        }
        with_managed(|scope| Box::pin(async move {
            for (case_index, (cached_before_initialize, close_control, established_first)) in [
                (true, false, false),
                (false, false, false),
                (false, false, true),
                (true, true, false),
                (false, true, false),
                (false, true, true),
            ].into_iter().enumerate() {
                let directory = scope.tempdir();
                let inspection = scope.inspect_next_runner(TEST_NAME);
                let (first_prompt, prompt_started) = oneshot::channel();
                let mut first_prompt = Some(first_prompt);
                let (config, mut native) = scope.native_peer(directory.path().into(), "gemini", "gemini", move |request| {
                    let id = request.get("id")?;
                    match request["method"].as_str().unwrap() {
                        "initialize" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1,"agentCapabilities":{}}})),
                        "session/new" => Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"sid-adopted"}})),
                        "session/prompt" => {
                            assert_eq!(request["params"]["sessionId"], "sid-adopted");
                            if let Some(started) = first_prompt.take() {
                                started.send(id.clone()).unwrap();
                                None
                            } else {
                                Some(serde_json::json!({"jsonrpc":"2.0","id":id,"result":{"stopReason":"end_turn"}}))
                            }
                        }
                        other => panic!("no implicit native establishment during adoption: {other}"),
                    }
                });
                let original = scope.spawn(config, "adopted-barrier").await.unwrap();
                original.ready().await;
                original.send_prompt("genuine in-flight original", &[]).await.unwrap();
                let prompt_id = tokio::time::timeout(Duration::from_secs(10), prompt_started).await.unwrap().unwrap();
                let (fence, release, entered) = scope.gate();
                let (native_seen, received_native) = oneshot::channel();
                scope.configure_sdk(SdkTestOptions {
                    bootstrap_fence: (!established_first).then_some(fence),
                    native_session_seen: Some(native_seen),
                    ..Default::default()
                });
                let mut client = scope.adopt_with(original, true, || async {
                    if cached_before_initialize {
                        tokio::time::timeout(Duration::from_secs(10), async {
                            // Detach joins the SDK, but the real runner still
                            // has to observe that socket's EOF. No original
                            // writer may consume this before-init backlog.
                            loop {
                                let snapshot = inspection.query("snapshot").await;
                                if snapshot["attached"] == false {
                                    assert_eq!(snapshot["queued_frames"], 0, "the original SDK left no earlier queue frames");
                                    assert_eq!(snapshot["retained_bytes"], 0, "the original prompt has not completed yet");
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                            send_adopted_backlog(&native, &prompt_id).await;
                            loop {
                                let snapshot = inspection.query("snapshot").await;
                                let completions = snapshot["queued_prompt_completions"].as_array().unwrap();
                                if let Some(completion) = completions.iter().find(|entry| entry[0] == prompt_id) {
                                    assert_eq!(snapshot["attached"], false, "no second SDK has initialized");
                                    assert_eq!(completions.len(), 1, "one real original outcome is cached");
                                    assert_eq!(completion[1], 12, "native chunks precede their cached outcome");
                                    assert_eq!(completion[2], 12, "all twelve native notifications are cached");
                                    assert_eq!(completion[3], true, "the cached original result is the native error");
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(5)).await;
                            }
                        }).await.expect("the genuine native outcome is cached before the new SDK initializes");
                    }
                }).await;
                let actual = client.actual();
                let control = actual.test_control();
                let state = actual.test_connection_state();
                let terminal = state.test_terminal.get().unwrap().clone();
                let in_flight = state.test_in_flight.get().unwrap().clone();
                let task = actual.observe_sdk_task();
                let adopted_turn = terminal.current_turn();
                assert_ne!(control.peer_identity().pid, std::process::id());
                let retain = |events: &mut Vec<Event>, event: Event| {
                    if matches!(&event, Event::AcpSessionAssigned { connection_ready: true, .. }) {
                        assert!(actual.establishment_projection().and_then(|projection| projection.applied()).is_some(),
                            "a ready assignment requires the actual Applied projection");
                    }
                    events.push(event);
                };
                let mut events = Vec::new();
                const PHASES: &[&str] = &[
                    "bootstrap_gate",
                    "native_seen",
                    "ready_before_pressure",
                    "assignment_before_pressure",
                    "drain_receive_pause",
                    "backlog_written",
                    "ordered_reverse_denial",
                    "control_close_requested",
                    "control_rx_closed",
                    "gate_released",
                    "failed_stopped",
                    "fatal_observed",
                    "foreground_finished",
                    "eof_cancel_refusal",
                    "eof_prompt_refusal",
                    "prepare_original_retirement",
                    "sdk_task_joined",
                    "retired",
                    "forged_call_written",
                    "forged_call_rejected",
                    "plan_written",
                    "plan_observed",
                    "ready_before_next_prompt",
                    "next_prompt_sent",
                    "next_prompt_stopped",
                ];
                let phase = std::sync::atomic::AtomicUsize::new(0);
                let completed = tokio::time::timeout(Duration::from_secs(10), async {
                    if !established_first { entered.await.unwrap(); }
                    phase.store(1, AtomicOrdering::Relaxed);
                    received_native.await.unwrap();
                    if established_first {
                        phase.store(2, AtomicOrdering::Relaxed);
                        client.ready().await;
                        phase.store(3, AtomicOrdering::Relaxed);
                        loop {
                            let event = client.next_event().await.unwrap();
                            let assigned = matches!(event, Event::AcpSessionAssigned { .. });
                            retain(&mut events, event);
                            if assigned { break; }
                        }
                    }
                    // Fill the real SDK lane while its sole drain is paused.
                    phase.store(4, AtomicOrdering::Relaxed);
                    let pause = client.pause_drain().await;
                    let sender = actual.test_event_queue().upgrade().unwrap();
                    while let Some(result) = sender.historical().send(
                        Event::AuthStatusUpdated { status: None },
                    ).now_or_never() {
                        result.expect("actual SDK event lane remains open");
                    }
                    assert_eq!(sender.capacity(), 0, "public queue is full before adopted publication");
                    phase.store(5, AtomicOrdering::Relaxed);
                    if !cached_before_initialize { send_adopted_backlog(&native, &prompt_id).await; }
                    phase.store(6, AtomicOrdering::Relaxed);
                    let barrier_id = 10_000_i64 + case_index as i64;
                    let barrier_path = directory.path().join("denied-ordering-barrier");
                    assert!(!barrier_path.exists());
                    native.send(serde_json::json!({
                        "jsonrpc": "2.0", "id": barrier_id,
                        "method": "fs/write_text_file",
                        "params": {
                            "sessionId": "not-the-selected-session",
                            "path": barrier_path,
                            "content": "must not be written"
                        }
                    })).await;
                    let denied = native.reply().await;
                    assert_eq!(denied["id"], barrier_id);
                    assert_eq!(denied["error"]["code"], -32602);
                    assert!(!barrier_path.exists());
                    assert_eq!(sender.capacity(), 0);
                    assert!(!terminal.claimed(), "receipt is not publication under backpressure");
                    assert_eq!(terminal.current_turn(), adopted_turn);
                    assert!(in_flight.load(AtomicOrdering::Acquire));
                    let operation_scope = match &*control.completion.lock().unwrap() {
                        PromptCompletion::Adopted(Some(head)) => head.scope.clone(),
                        _ => panic!("unpublished completion must retain its actual adopted head"),
                    };
                    while let Ok(event) = client.try_recv() {
                        assert!(!matches!(&event,
                            Event::PromptRuntimeError { .. }
                                | Event::Stopped { .. }
                                | Event::AcpConnectionUnavailable { .. }
                                | Event::AgentStartupError { .. }),
                            "completion crossed the actual publication boundary: {event:?}");
                        retain(&mut events, event);
                    }
                    if close_control {
                        phase.store(7, AtomicOrdering::Relaxed);
                        inspection.query("close-control").await;
                        phase.store(8, AtomicOrdering::Relaxed);
                        loop {
                            let ended = control.handshake_rx.lock().await.is_closed();
                            if ended { break; }
                            tokio::task::yield_now().await;
                        }
                    }
                    phase.store(9, AtomicOrdering::Relaxed);
                    if !established_first { assert!(release.release()); }
                    drop(pause);
                    drop(sender);
                    phase.store(10, AtomicOrdering::Relaxed);
                    loop {
                        let event = client.next_event().await.unwrap();
                        let stopped = matches!(event, Event::Stopped { .. });
                        retain(&mut events, event);
                        if stopped { break; }
                    }
                    let diagnostic = events.iter().position(|event| matches!(event, Event::PromptRuntimeError { .. })).unwrap();
                    assert_eq!(events[..diagnostic].iter().filter(|event| matches!(event, Event::AgentMessageChunk { .. })).count(), 12);
                    assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { .. })).count(), 1);
                    assert!(matches!(events.last(), Some(Event::Stopped { reason }) if reason == "prompt_failed"));
                    assert_eq!(terminal.current_turn(), adopted_turn, "buffered output cannot invent a main-turn epoch");
                    assert!(!in_flight.load(AtomicOrdering::Acquire));
                    assert!(!serde_json::to_string(&events).unwrap().contains("privateTokenMarker"));
                    if close_control {
                        phase.store(11, AtomicOrdering::Relaxed);
                        loop {
                            let event = client.next_event().await.unwrap();
                            let fatal = matches!(event, Event::AcpConnectionUnavailable { .. });
                            retain(&mut events, event);
                            if fatal { break; }
                        }
                        phase.store(12, AtomicOrdering::Relaxed);
                        while !actual.connection_state.driver_finished() {
                            tokio::task::yield_now().await;
                        }
                        let expected = actual.connection_unavailable().expect("actual sticky Control EOF");
                        assert!(matches!(expected, crate::acp::state::AcpConnectionUnavailability {
                            source: crate::acp::state::AcpConnectionSource::Control,
                            cause: crate::acp::state::AcpConnectionCause::IncomingEnded, ..
                        }));
                        assert!(!control.peer_process.exited().unwrap());
                        assert!(!task.is_finished(), "socket EOF is not the owned child's death");
                        phase.store(13, AtomicOrdering::Relaxed);
                        let _ = control.cancel(operation_scope.clone()).await;
                        let request: agent_client_protocol::schema::v1::PromptRequest =
                            serde_json::from_value(serde_json::json!({
                                "sessionId":"sid-adopted",
                                "prompt":[{"type":"text","text":"must be refused after observed EOF"}]
                            })).unwrap();
                        phase.store(14, AtomicOrdering::Relaxed);
                        let error = match control.prompt(serde_json::to_value(request).unwrap(), operation_scope).await {
                            Err(error) => error,
                            Ok(_) => panic!("known Control EOF admitted a native prompt"),
                        };
                        assert!(matches!(&error, AcpError::ConnectionUnavailable(observation) if *observation == expected),
                            "unexpected EOF admission error: {error:?}");
                        let requests = native.requests();
                        for (method, count) in [("session/new", 1), ("session/prompt", 1),
                            ("session/load", 0), ("session/fork", 0), ("session/cancel", 0)] {
                            assert_eq!(requests.iter().filter(|request| request.as_str() == method).count(), count, "{method}");
                        }
                        phase.store(15, AtomicOrdering::Relaxed);
                        client.prepare_for_sdk_shutdown().await.unwrap();
                        actual.connection_shutdown.cancel();
                        phase.store(16, AtomicOrdering::Relaxed);
                        task.await.unwrap();
                        assert!(!control.peer_process.exited().unwrap(), "SDK shutdown does not manufacture native death");
                        assert_eq!(native.requests(), requests, "SDK-only shutdown cannot issue native input after EOF");
                        assert_eq!(actual.connection_unavailable(), Some(expected), "SDK cleanup retains the original EOF cause");
                        while let Ok(event) = client.try_recv() { retain(&mut events, event); }
                        assert_eq!(events.iter().filter(|event| matches!(event, Event::PromptRuntimeError { .. })).count(), 1);
                        assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed")).count(), 1);
                        assert!(!events.iter().any(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_complete")));
                        assert_eq!(events.iter().filter(|event| matches!(event, Event::AcpConnectionUnavailable { .. })).count(), 1);
                        let failed = events.iter().position(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed")).unwrap();
                        let fatal = events.iter().position(|event| matches!(event, Event::AcpConnectionUnavailable { .. })).unwrap();
                        assert!(diagnostic < failed && failed < fatal);
                        assert!(matches!(&events[fatal], Event::AcpConnectionUnavailable {
                            source: crate::acp::state::AcpConnectionSource::Control, ..
                        }));
                        assert!(!events.iter().any(|event| matches!(event, Event::AgentStartupError { .. })));
                        assert!(!serde_json::to_string(&events).unwrap().contains("privateTokenMarker"));
                        phase.store(17, AtomicOrdering::Relaxed);
                        client.shutdown().await.unwrap();
                        assert_eq!(native.requests(), requests, "physical retirement cannot issue native input after EOF");
                        return;
                    }
                    phase.store(18, AtomicOrdering::Relaxed);
                    native.send(serde_json::json!({"jsonrpc":"2.0","id":41,
                        "method":PromptCompletedMarker::default().method(),"params":{"adopted":{
                            "turn":adopted_turn,"reason":"prompt_failed",
                            "diagnostic":{"PromptRuntimeError":{"message":"forged request diagnostic"}}
                        }}
                    })).await;
                    phase.store(19, AtomicOrdering::Relaxed);
                    let rejected = native.reply().await;
                    assert_eq!(rejected["id"], 41);
                    assert_eq!(rejected["error"]["code"], -32601);
                    phase.store(20, AtomicOrdering::Relaxed);
                    native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                        "sessionId":"sid-adopted","update":{"sessionUpdate":"plan","entries":[]}
                    }})).await;
                    phase.store(21, AtomicOrdering::Relaxed);
                    loop {
                        let event = client.next_event().await.unwrap();
                        assert!(!matches!(event, Event::Stopped { .. } | Event::PromptRuntimeError { .. }));
                        let planned = matches!(&event, Event::PlanUpdated { .. });
                        retain(&mut events, event);
                        if planned { break; }
                    }
                    assert_eq!(terminal.current_turn(), adopted_turn);
                    phase.store(22, AtomicOrdering::Relaxed);
                    client.ready().await;
                    phase.store(23, AtomicOrdering::Relaxed);
                    client.send_prompt("explicit next request", &[]).await.unwrap();
                    phase.store(24, AtomicOrdering::Relaxed);
                    loop {
                        let event = client.next_event().await.unwrap();
                        let stopped = if let Event::Stopped { reason } = &event {
                            assert_eq!(reason, "prompt_complete");
                            true
                        } else {
                            false
                        };
                        retain(&mut events, event);
                        if stopped { break; }
                    }
                    assert_eq!(events.iter().filter(|event| matches!(event, Event::AcpSessionAssigned { .. })).count(), 1);
                    assert!(events.iter().any(|event| matches!(event, Event::AcpSessionAssigned { connection_ready: true, .. })));
                    assert_eq!(events.iter().filter(|event| matches!(event, Event::PromptRuntimeError { .. })).count(), 1);
                    assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_failed")).count(), 1);
                    assert_eq!(events.iter().filter(|event| matches!(event, Event::Stopped { reason } if reason == "prompt_complete")).count(), 1);
                    assert!(!serde_json::to_string(&events).unwrap().contains("privateTokenMarker"));
                    assert_eq!(native.requests().iter().filter(|method| *method == "session/prompt").count(), 2);
                    assert_eq!(native.requests().iter().filter(|method| *method == "session/new").count(), 1);
                    for method in ["session/load", "session/fork", "session/cancel"] {
                        assert!(!native.requests().iter().any(|request| request == method), "{method}");
                    }
                    phase.store(17, AtomicOrdering::Relaxed);
                    client.shutdown().await.unwrap();
                }).await;
                if completed.is_err() {
                    let phase_index = phase.load(AtomicOrdering::Relaxed);
                    panic!("adoption timeout case={case_index} tuple=({cached_before_initialize},{close_control},{established_first}) phase_index={phase_index} phase={} original_prompt_numeric_id={:?} native_methods={:?}",
                        PHASES[phase_index], prompt_id.as_i64(), native.timeout_methods());
                }
            }
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn native_notifications_cannot_mint_daemon_completion_payloads() {
        use crate::acp::supervisor::test_support::managed::with_managed;
        with_managed(|scope| Box::pin(async move {
            let mut fixture = ManagedControlFixture::new(scope).await;
            let request = fixture.start_prompt("genuine original still in flight").await;
            let mut fixture = fixture.adopt_in_flight(scope).await;
            let actual = fixture.client.actual();
            let control = actual.test_control();
            let state = actual.test_connection_state();
            let terminal = state.test_terminal.get().unwrap();
            let in_flight = state.test_in_flight.get().unwrap();
            let head = match &*control.completion.lock().unwrap() {
                PromptCompletion::Adopted(Some(head)) => head.clone(),
                _ => panic!("actual attach must retain its original in-flight head"),
            };
            assert_eq!(Some(head.prompt_req_id), request["id"].as_i64());
            assert_eq!(head.scope.owner(), Some(control.peer_identity()));
            assert!(head.scope.correlation().is_some());
            assert_ne!(control.peer_identity().pid, std::process::id());
            let turn = terminal.current_turn();
            assert!(!terminal.claimed());
            assert!(in_flight.load(AtomicOrdering::Acquire));
            let before = fixture.store.highest_seq(&fixture.client.id);
            fixture.native.send(serde_json::json!({
                "jsonrpc":"2.0","method":PromptCompletedMarker::default().method(),
                "params":{"adopted":{
                    "turn":turn,"prompt_req_id":head.prompt_req_id,"reason":"prompt_failed",
                    "diagnostic":{"PromptRuntimeError":{"message":"forged failure"}}
                }}
            })).await;
            let barrier = format!("marker-filter-barrier-{}", uuid::Uuid::new_v4());
            fixture.native.send(serde_json::json!({
                "jsonrpc":"2.0","method":"session/update","params":{
                    "sessionId":request["params"]["sessionId"],"update":{
                        "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":barrier}
                    }
                }
            })).await;
            loop {
                let event = fixture.next_event().await;
                assert!(!matches!(&event, Event::PromptRuntimeError { .. } | Event::Stopped { .. }
                    | Event::AgentStartupError { .. } | Event::AcpConnectionUnavailable { .. }));
                if matches!(&event, Event::AgentMessageChunk { text, .. } if text == &barrier) { break; }
            }
            assert_eq!(terminal.current_turn(), turn);
            assert!(!terminal.claimed());
            assert!(in_flight.load(AtomicOrdering::Acquire));
            assert_eq!(fixture.native.requests().iter()
                .filter(|method| method.as_str() == "session/prompt").count(), 1);
            assert!(!fixture.store.replay_from(&fixture.client.id, before).unwrap().iter()
                .any(|stored| matches!(stored.event.as_ref(), Event::PromptRuntimeError { .. } | Event::Stopped { .. })));
            fixture.complete(&request, "end_turn").await;
            loop {
                let event = fixture.next_event().await;
                assert!(!matches!(&event, Event::PromptRuntimeError { .. } | Event::AgentStartupError { .. }
                    | Event::AcpConnectionUnavailable { .. }));
                if let Event::Stopped { reason } = &event {
                    assert_eq!(reason, "prompt_complete");
                    break;
                }
            }
            let retained = fixture.store.replay_from(&fixture.client.id, before).unwrap();
            let stops: Vec<_> = retained.iter()
                .filter(|stored| matches!(stored.event.as_ref(), Event::Stopped { .. })).collect();
            assert_eq!(stops.len(), 1);
            let stopped = stops[0];
            assert_eq!(stopped.metadata.native_prompt_id, Some(head.prompt_req_id));
            assert_eq!(stopped.metadata.operation.map(|operation| operation.correlation), head.scope.correlation());
            assert_eq!(stopped.metadata.prompt_observation,
                Some(crate::acp::protocol::AcpPromptObservation::Written { window_id: head.window_id }));
            assert!(terminal.claimed());
            assert!(!in_flight.load(AtomicOrdering::Acquire));
            fixture.client.shutdown().await.unwrap();
        })).await;
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn resumed_prompt_completes_only_for_its_own_runner_request() {
        use crate::acp::event_sender::EventScope;
        use crate::acp::supervisor::test_support::managed::with_managed;

        async fn own_stop(fixture: &ManagedControlFixture, request: &serde_json::Value) {
            real_control_deadline(async {
                loop {
                    let events = fixture.store.replay_from(&fixture.client.id, 0).unwrap();
                    if events.iter().any(|stored| {
                        stored.metadata.scope == EventScope::OperationCurrent
                            && stored.metadata.native_prompt_id == request["id"].as_i64()
                            && matches!(stored.event.as_ref(), Event::Stopped { reason } if reason == "prompt_complete")
                    }) { break; }
                    tokio::task::yield_now().await;
                }
            }).await;
        }

        async fn drained_old_completion(
            fixture: &mut ManagedControlFixture,
            callback_id: &str,
            forbidden: &std::path::Path,
        ) {
            fixture.native.send(serde_json::json!({"jsonrpc":"2.0","id":callback_id,
                "method":"fs/write_text_file","params":{
                    "sessionId":"foreign-correlation-barrier","path":forbidden,"content":"must not be written"
                }})).await;
            let reply = real_control_deadline(fixture.native.reply()).await;
            assert_eq!(reply["id"], callback_id);
            assert_eq!(reply["error"]["code"], control_protocol::INVALID_PARAMS);
            assert!(!forbidden.exists());
            // A direct refusal proves reader progress, not SDK or parent application.
            let barrier = format!("parent-processed-{callback_id}");
            fixture.native.send(serde_json::json!({"jsonrpc":"2.0","method":"session/update","params":{
                "sessionId":"foreign-correlation-barrier","update":{
                    "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":barrier}
                }
            }})).await;
            real_control_deadline(async {
                loop {
                    let events = fixture.store.replay_from(&fixture.client.id, 0).unwrap();
                    if let Some(stored) = events.iter().find(|stored| {
                        matches!(stored.event.as_ref(),
                            Event::AgentMessageChunk { text, .. } if text == &barrier
                        )
                    }) {
                        assert_eq!(stored.metadata.scope, EventScope::HistoryOnly);
                        assert_eq!(
                            stored
                                .metadata
                                .reported_session_id
                                .as_ref()
                                .map(|id| id.0.as_ref()),
                            Some("foreign-correlation-barrier")
                        );
                        assert!(stored.metadata.operation.is_none());
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await;
        }

        with_managed(|scope| {
            Box::pin(async move {
                for old_first in [true, false] {
                    let directory = scope.tempdir();
                    let forbidden = directory.path().join("foreign-write");
                    let mut fixture = ManagedControlFixture::new(scope).await;
                    let old = fixture
                        .start_prompt("actual original before reattachment")
                        .await;
                    let mut fixture = fixture.adopt_in_flight(scope).await;
                    let actual = fixture.client.actual();
                    let control = actual.test_control();
                    let state = actual.test_connection_state();
                    let terminal = state.test_terminal.get().unwrap().clone();
                    let in_flight = state.test_in_flight.get().unwrap().clone();
                    let before = fixture.store.highest_seq(&fixture.client.id);
                    let new = fixture.start_prompt("actual new local request").await;
                    assert_ne!(old["id"], new["id"]);
                    let (new_scope, new_window) = real_pending_scope(&control, &new).await;
                    let turn = terminal.current_turn();
                    if old_first {
                        fixture.complete(&old, "cancelled").await;
                        drained_old_completion(&mut fixture, "old-first-barrier", &forbidden).await;
                        let still_pending = real_pending_scope(&control, &new).await;
                        assert_eq!(still_pending, (new_scope.clone(), new_window));
                        assert_eq!(terminal.current_turn(), turn);
                        assert!(!terminal.claimed());
                        assert!(in_flight.load(AtomicOrdering::Acquire));
                    }
                    fixture.complete(&new, "max_tokens").await;
                    own_stop(&fixture, &new).await;
                    assert_eq!(terminal.current_turn(), turn);
                    assert!(terminal.claimed());
                    assert!(!in_flight.load(AtomicOrdering::Acquire));
                    if !old_first {
                        fixture.complete(&old, "cancelled").await;
                        drained_old_completion(&mut fixture, "new-first-barrier", &forbidden).await;
                        assert_eq!(terminal.current_turn(), turn);
                        assert!(terminal.claimed());
                        assert!(!in_flight.load(AtomicOrdering::Acquire));
                    }
                    let following = fixture.start_prompt("following real local request").await;
                    let (following_scope, following_window) =
                        real_pending_scope(&control, &following).await;
                    assert_ne!(following["id"], new["id"]);
                    assert_ne!(following_scope.correlation(), new_scope.correlation());
                    assert_ne!(following_window, new_window);
                    fixture.complete(&following, "end_turn").await;
                    own_stop(&fixture, &following).await;
                    let stored = fixture
                        .store
                        .replay_from(&fixture.client.id, before)
                        .unwrap();
                    let current: Vec<_> = stored
                        .iter()
                        .filter(|stored| {
                            stored.metadata.scope == EventScope::OperationCurrent
                                && matches!(stored.event.as_ref(), Event::Stopped { .. })
                        })
                        .collect();
                    assert_eq!(
                        current.len(),
                        2,
                        "only the two actual local requests own current completion"
                    );
                    for (stored, request, scope, window) in [
                        (current[0], &new, &new_scope, new_window),
                        (current[1], &following, &following_scope, following_window),
                    ] {
                        assert_eq!(stored.metadata.native_prompt_id, request["id"].as_i64());
                        assert_eq!(
                            stored
                                .metadata
                                .operation
                                .map(|operation| operation.correlation),
                            scope.correlation()
                        );
                        assert_eq!(
                            stored.metadata.prompt_observation,
                            Some(crate::acp::protocol::AcpPromptObservation::Written {
                                window_id: window
                            })
                        );
                    }
                    assert!(!stored.iter().any(|stored| stored.metadata.scope
                        == EventScope::OperationCurrent
                        && stored.metadata.native_prompt_id == old["id"].as_i64()
                        && matches!(stored.event.as_ref(), Event::Stopped { .. })));
                    let methods = fixture.native.requests();
                    assert_eq!(
                        methods
                            .iter()
                            .filter(|method| *method == "session/new")
                            .count(),
                        1
                    );
                    assert_eq!(
                        methods
                            .iter()
                            .filter(|method| *method == "session/prompt")
                            .count(),
                        3
                    );
                    assert!(!methods.iter().any(|method| matches!(
                        method.as_str(),
                        "session/load" | "session/fork" | "session/cancel"
                    )));
                    fixture.client.shutdown().await.unwrap();
                }
            })
        })
        .await;
    }
}
