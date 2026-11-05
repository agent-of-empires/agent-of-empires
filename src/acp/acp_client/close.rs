//! Capability-gated session release, distinct from deleting persisted history.

use crate::acp::establishment::NativeRequestContext;
use agent_client_protocol::schema::v1::{CloseSessionRequest, ErrorCode, SessionId};
use agent_client_protocol::{Agent, ConnectionTo};
use tokio::sync::oneshot;

use super::connection::ConnectionState;
use super::delete::{truncate_for_log, ACP_DELETE_ERROR_MSG_MAX};
use super::session_identity::{ordered_native_request, OrderedRequestResult};
use super::spawn::scrub_stderr_secrets;
use std::sync::Arc;

pub(super) const CLOSE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

#[derive(Debug)]
pub enum CloseSessionOutcome {
    Closed,
    Unsupported,
    TimedOut,
    Failed(String),
}

pub(super) fn dispatch_close(
    connection: &ConnectionTo<Agent>,
    state: &Arc<ConnectionState>,
    session_id: SessionId,
    capable: bool,
    respond_to: oneshot::Sender<CloseSessionOutcome>,
    context: Option<NativeRequestContext>,
) {
    if !capable {
        let _ = respond_to.send(CloseSessionOutcome::Unsupported);
        return;
    }
    if state.control.get().is_some()
        && context
            .as_ref()
            .and_then(|context| context.receipt.as_deref())
            .and_then(|receipt| receipt.session_id())
            != Some(session_id.0.as_ref())
    {
        let _ = respond_to.send(CloseSessionOutcome::Failed(
            "NotIssued: close target differs from frozen native scope".into(),
        ));
        return;
    }
    let request = match ordered_native_request(
        connection,
        state,
        context,
        CloseSessionRequest::new(session_id),
    ) {
        Ok(request) => request,
        Err(error) => {
            let _ = respond_to.send(CloseSessionOutcome::Failed(format!("NotIssued: {error}")));
            return;
        }
    };
    tokio::spawn(async move {
        let outcome = match tokio::time::timeout(CLOSE_TIMEOUT, request.wait()).await {
            Ok(OrderedRequestResult::Correlated(Ok(_))) => CloseSessionOutcome::Closed,
            Ok(OrderedRequestResult::Correlated(Err(error)))
                if error.code == ErrorCode::MethodNotFound =>
            {
                CloseSessionOutcome::Unsupported
            }
            Ok(
                OrderedRequestResult::Correlated(Err(error)) | OrderedRequestResult::Refused(error),
            ) => CloseSessionOutcome::Failed(truncate_for_log(
                &scrub_stderr_secrets(&error.message),
                ACP_DELETE_ERROR_MSG_MAX,
            )),
            Ok(OrderedRequestResult::Unavailable(_)) => {
                CloseSessionOutcome::Failed("ACP connection unavailable".into())
            }
            Ok(OrderedRequestResult::Abandoned) => {
                CloseSessionOutcome::Failed("Close response was not observed".into())
            }
            Err(_) => CloseSessionOutcome::TimedOut,
        };
        let _ = respond_to.send(outcome);
    });
}
