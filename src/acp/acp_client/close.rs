//! Capability-gated session release, distinct from deleting persisted history.

use agent_client_protocol::schema::v1::{CloseSessionRequest, ErrorCode, SessionId};
use agent_client_protocol::{Agent, ConnectionTo};
use tokio::sync::oneshot;

use super::delete::{truncate_for_log, ACP_DELETE_ERROR_MSG_MAX};
use super::spawn::scrub_stderr_secrets;

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
    session_id: SessionId,
    capable: bool,
    respond_to: oneshot::Sender<CloseSessionOutcome>,
) {
    if !capable {
        let _ = respond_to.send(CloseSessionOutcome::Unsupported);
        return;
    }
    let sent = connection.send_request(CloseSessionRequest::new(session_id));
    tokio::spawn(async move {
        let outcome = match tokio::time::timeout(CLOSE_TIMEOUT, sent.block_task()).await {
            Ok(Ok(_)) => CloseSessionOutcome::Closed,
            Ok(Err(error)) if error.code == ErrorCode::MethodNotFound => {
                CloseSessionOutcome::Unsupported
            }
            Ok(Err(error)) => CloseSessionOutcome::Failed(truncate_for_log(
                &scrub_stderr_secrets(&error.message),
                ACP_DELETE_ERROR_MSG_MAX,
            )),
            Err(_) => CloseSessionOutcome::TimedOut,
        };
        let _ = respond_to.send(outcome);
    });
}
