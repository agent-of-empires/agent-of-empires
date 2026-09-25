//! Rename a session on a remote daemon.
//!
//! Runs on a worker: a tied rename moves a worktree directory on the other
//! machine before the daemon answers, which must not stall the TUI.

use std::sync::mpsc::TryRecvError;

use reqwest::StatusCode;

use crate::daemon::{DaemonClient, DaemonClientError, RenameSessionBody};
use crate::session::Status;
use crate::tui::worker::Worker;

/// Whether the delete key's sibling, rename, applies to a remote row. Decided
/// from the row alone: this machine cannot resolve the other daemon's config.
pub(crate) fn can_rename(row: &crate::daemon::SessionResponse) -> bool {
    !matches!(
        Status::from_api_str(&row.status),
        Some(Status::Creating) | Some(Status::Deleting)
    )
}

pub(crate) struct RenameRequest {
    pub remote: String,
    pub client: DaemonClient,
    pub session_id: String,
    pub title: String,
}

/// What one remote rename produced. Both messages name the remote, so the
/// caller shows them as they are.
pub(crate) enum RenameResult {
    Done(String),
    Failed(String),
}

pub struct RemoteRename {
    worker: Worker<RenameRequest, RenameResult>,
}

impl RemoteRename {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        Self {
            worker: Worker::spawn(
                "aoe-remote-rename",
                move |request: RenameRequest| match runtime.as_ref() {
                    Ok(rt) => rt.block_on(run_rename(&request)),
                    Err(e) => RenameResult::Failed(format!("{}: no runtime: {e}", request.remote)),
                },
            ),
        }
    }

    pub(crate) fn request(&self, request: RenameRequest) {
        self.worker.request(request);
    }

    pub(crate) fn try_recv(&self) -> Result<RenameResult, TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for RemoteRename {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_rename(request: &RenameRequest) -> RenameResult {
    let remote = request.remote.as_str();
    // The branch toggle belongs to the machine that owns the worktree: this
    // one cannot read the branch, its upstream, or whether the remote ties
    // directories to titles at all, so it never asks for the rename.
    let body = RenameSessionBody {
        title: request.title.clone(),
        rename_branch: false,
    };
    match request
        .client
        .rename_session_unpinned(&request.session_id, &body)
        .await
    {
        Ok(row) => RenameResult::Done(format!("Renamed to '{}' on {remote}", row.title)),
        Err(e) => RenameResult::Failed(rename_failure_message(remote, &e)),
    }
}

/// Why a rename failed, from the status and the `aoe-error-code` header: the
/// daemon's error body is never read.
fn rename_failure_message(remote: &str, error: &DaemonClientError) -> String {
    let detail = match error {
        DaemonClientError::Status {
            status: StatusCode::NOT_FOUND,
            code: None,
            ..
        } => "the session is already gone there (HTTP 404)".to_string(),
        // Two rejections share this status and the body that tells them apart
        // is never read, so the message names both.
        DaemonClientError::Status {
            status: StatusCode::CONFLICT,
            code: None,
            ..
        } => "it refused the name (HTTP 409): another session there uses it, \
              or the session must be stopped before its worktree can move"
            .to_string(),
        DaemonClientError::Status {
            status: StatusCode::BAD_REQUEST,
            code: None,
            ..
        } => "it rejected the title (HTTP 400)".to_string(),
        other => other.summary(),
    };
    format!("{remote}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: serde_json::Value) -> crate::daemon::SessionResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_row_mid_lifecycle_has_no_title_to_edit() {
        for (status, expected) in [
            ("Creating", false),
            ("Deleting", false),
            ("Running", true),
            ("Idle", true),
        ] {
            let row = row(serde_json::json!({"id": "a", "status": status}));
            assert_eq!(can_rename(&row), expected, "{status}");
        }
    }

    #[test]
    fn a_failed_rename_is_named_by_its_remote_and_described_from_its_status() {
        let status = |status, code| DaemonClientError::Status {
            status,
            code,
            body: "server text".into(),
            truncated: false,
        };
        for (error, expected) in [
            (status(StatusCode::NOT_FOUND, None), "already gone there"),
            (status(StatusCode::CONFLICT, None), "another session there"),
            (status(StatusCode::BAD_REQUEST, None), "rejected the title"),
            (
                status(
                    StatusCode::FORBIDDEN,
                    Some(crate::daemon::ApiErrorCode::ReadOnly),
                ),
                "read-only",
            ),
        ] {
            let message = rename_failure_message("mini", &error);
            assert!(message.starts_with("mini: "), "{message}");
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("server text"), "{message}");
        }
    }
}
