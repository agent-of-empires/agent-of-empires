//! Background resolver for structured-session approval requests.
//!
//! The home view must not wait for daemon HTTP while processing a key. This
//! worker resolves the nonce selected in the existing permission dialog and
//! returns the outcome for the event loop to apply.

use std::sync::mpsc::TryRecvError;

use crate::acp::client::{require_daemon, HttpClient, HttpError};
use crate::acp::protocol::ApprovalDecisionWire;
use crate::tui::worker::Worker;

pub(super) struct ApprovalRequest {
    pub session_id: String,
    pub nonce: String,
    pub decision: ApprovalDecisionWire,
}

pub(super) enum ApprovalResolution {
    Resolved,
    Gone,
    Failed(String),
}

pub(super) struct ApprovalResult {
    pub session_id: String,
    pub nonce: String,
    pub resolution: ApprovalResolution,
}

async fn resolve(request: ApprovalRequest) -> ApprovalResult {
    let session_id = request.session_id;
    let nonce = request.nonce;
    let resolution = match require_daemon().await {
        Ok(endpoint) => match HttpClient::new(endpoint) {
            Ok(client) => match client
                .resolve_approval(&session_id, &nonce, request.decision)
                .await
            {
                Ok(()) => ApprovalResolution::Resolved,
                Err(HttpError::ApprovalGone) => ApprovalResolution::Gone,
                Err(error) => ApprovalResolution::Failed(error.to_string()),
            },
            Err(error) => ApprovalResolution::Failed(error.to_string()),
        },
        Err(error) => ApprovalResolution::Failed(error.to_string()),
    };
    ApprovalResult {
        session_id,
        nonce,
        resolution,
    }
}

/// Background worker for resolving approval choices from the home view.
pub(super) struct StructuredApprovalPoller {
    worker: Worker<ApprovalRequest, ApprovalResult>,
}

impl StructuredApprovalPoller {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Err(error) = &runtime {
            tracing::warn!(
                target: "tui.structured_approval",
                "runtime build failed; structured approval responses are unavailable: {error}"
            );
        }
        Self {
            worker: Worker::spawn("aoe-structured-approval", move |request| {
                match runtime.as_ref() {
                    Ok(runtime) => runtime.block_on(resolve(request)),
                    Err(_) => ApprovalResult {
                        session_id: request.session_id,
                        nonce: request.nonce,
                        resolution: ApprovalResolution::Failed(
                            "Cannot start the structured approval worker.".to_string(),
                        ),
                    },
                }
            }),
        }
    }

    pub fn request_resolve(&self, request: ApprovalRequest) {
        self.worker.request(request);
    }

    pub fn try_recv_result(&self) -> Result<ApprovalResult, TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for StructuredApprovalPoller {
    fn default() -> Self {
        Self::new()
    }
}
