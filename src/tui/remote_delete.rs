//! Trash or permanently delete a session on a remote daemon.
//!
//! Runs on a worker: the daemon answers only after it has torn down panes,
//! stopped a container or removed a worktree, which must not stall the TUI.

use std::sync::mpsc::TryRecvError;

use reqwest::StatusCode;

use crate::daemon::{
    DaemonClient, DaemonClientError, DeleteSessionBody, PurgeOutcome, SessionResponse,
    TrashOutcome, TrashRelocationOutcome, TrashSessionBody,
};
use crate::session::Status;
use crate::tui::dialogs::{DeleteDialogConfig, DeleteOptions, UnifiedDeleteDialog};
use crate::tui::worker::Worker;

/// What the delete key does to a remote row. Decided from the row alone: this
/// machine cannot resolve the other daemon's config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DeletePlan {
    /// Mid-create rows are inert, as they are locally.
    Inert,
    /// The remote trashes first, so confirm and move it to its trash.
    Trash,
    /// Open the permanent-delete dialog.
    Permanent,
}

pub(crate) fn plan_for(row: &SessionResponse) -> DeletePlan {
    if Status::from_api_str(&row.status) == Some(Status::Creating) {
        return DeletePlan::Inert;
    }
    // A row already in the remote's trash falls through, so the key deletes it
    // permanently there just as it does locally.
    if row.trashed_at.is_none() && row.cleanup_defaults.delete_to_trash {
        DeletePlan::Trash
    } else {
        DeletePlan::Permanent
    }
}

/// The permanent-delete dialog for a remote row, seeded from the row's own
/// `cleanup_defaults`. Its title names the machine, since the row sits beside
/// local ones.
pub(crate) fn delete_dialog(remote: &str, row: &SessionResponse) -> UnifiedDeleteDialog {
    let config = DeleteDialogConfig {
        worktree_branch: row
            .has_cleanable_worktree
            .then(|| row.branch.clone().or_else(|| row.workspace_branch.clone()))
            .flatten(),
        has_sandbox: row.is_sandboxed,
        // Resolving repo config would read this machine's copy of a path that
        // belongs to another one.
        project_path: None,
        is_scratch: row.scratch,
    };
    let defaults = &row.cleanup_defaults;
    UnifiedDeleteDialog::with_options(
        dialog_title(remote, row),
        config,
        DeleteOptions {
            delete_worktree: defaults.delete_worktree,
            force_delete: false,
            delete_branch: defaults.delete_branch,
            delete_sandbox: defaults.delete_sandbox,
            keep_scratch: false,
        },
    )
}

/// What the dialog asks about. Remote rows sit beside local ones in the
/// sidebar, so the prompt has to say which machine it acts on.
fn dialog_title(remote: &str, row: &SessionResponse) -> String {
    format!("{} on {remote}", row.title)
}

pub(crate) fn purge_body(options: &DeleteOptions) -> DeleteSessionBody {
    DeleteSessionBody {
        delete_worktree: options.delete_worktree,
        delete_branch: options.delete_branch,
        delete_sandbox: options.delete_sandbox,
        force_delete: options.force_delete,
        keep_scratch: options.keep_scratch,
    }
}

/// Which of the remote's two delete routes to call.
pub(crate) enum DeleteKind {
    Trash,
    Purge(DeleteSessionBody),
}

pub(crate) struct DeleteRequest {
    pub remote: String,
    pub client: DaemonClient,
    pub session_id: String,
    pub kind: DeleteKind,
}

/// What one remote delete produced. Both messages name the remote, so the
/// caller shows them as they are.
pub(crate) enum DeleteResult {
    /// The remote removed the row, or moved it to its own trash.
    Done(String),
    /// Nothing was removed.
    Failed(String),
}

pub struct RemoteDelete {
    worker: Worker<DeleteRequest, DeleteResult>,
}

impl RemoteDelete {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        Self {
            worker: Worker::spawn(
                "aoe-remote-delete",
                move |request: DeleteRequest| match runtime.as_ref() {
                    Ok(rt) => rt.block_on(run_delete(&request)),
                    Err(e) => DeleteResult::Failed(format!("{}: no runtime: {e}", request.remote)),
                },
            ),
        }
    }

    pub(crate) fn request(&self, request: DeleteRequest) {
        self.worker.request(request);
    }

    pub(crate) fn try_recv(&self) -> Result<DeleteResult, TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for RemoteDelete {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_delete(request: &DeleteRequest) -> DeleteResult {
    let remote = request.remote.as_str();
    match &request.kind {
        DeleteKind::Trash => {
            match request
                .client
                .trash_session_unpinned(&request.session_id, &TrashSessionBody::default())
                .await
            {
                Ok(outcome) => DeleteResult::Done(trashed_message(remote, &outcome)),
                Err(e) => DeleteResult::Failed(delete_failure_message(remote, &e)),
            }
        }
        DeleteKind::Purge(body) => {
            match request
                .client
                .purge_session_unpinned(&request.session_id, body)
                .await
            {
                Ok(outcome) => purged_result(remote, outcome),
                Err(e) => DeleteResult::Failed(delete_failure_message(remote, &e)),
            }
        }
    }
}

fn trashed_message(remote: &str, outcome: &TrashOutcome) -> String {
    match &outcome.relocation {
        TrashRelocationOutcome::Failed { reason } => {
            format!("Moved to {remote}'s trash; its worktree stayed put: {reason}")
        }
        _ => format!("Moved to {remote}'s trash"),
    }
}

/// A purge the daemon answered. `Kept` means the row survived, so it reads as
/// a failure however the request went over the wire.
fn purged_result(remote: &str, outcome: PurgeOutcome) -> DeleteResult {
    match outcome {
        PurgeOutcome::Deleted { cleanup_errors, .. } if !cleanup_errors.is_empty() => {
            DeleteResult::Done(format!(
                "Deleted on {remote}, but cleanup failed: {}",
                cleanup_errors.join("; ")
            ))
        }
        PurgeOutcome::Deleted { .. } => DeleteResult::Done(format!("Deleted on {remote}")),
        PurgeOutcome::Kept { messages, .. } if messages.is_empty() => {
            DeleteResult::Failed(format!("{remote} kept the session"))
        }
        PurgeOutcome::Kept { messages, .. } => DeleteResult::Failed(format!(
            "{remote} kept the session: {}",
            messages.join("; ")
        )),
    }
}

/// Why a delete failed, from the status and the `aoe-error-code` header: the
/// daemon's error body is never read.
fn delete_failure_message(remote: &str, error: &DaemonClientError) -> String {
    let detail = match error {
        DaemonClientError::Status {
            status: StatusCode::NOT_FOUND,
            code: None,
            ..
        } => "the session is already gone there (HTTP 404)".to_string(),
        DaemonClientError::Status {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: None,
            ..
        } => "its runtime is not ready (HTTP 503); try again once the daemon settles".to_string(),
        other => other.summary(),
    };
    format!("{remote}: {detail}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: serde_json::Value) -> SessionResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn the_delete_key_branches_on_the_rows_own_trash_state_and_defaults() {
        for (name, value, expected) in [
            (
                "mid-create",
                serde_json::json!({"id": "a", "status": "Creating"}),
                DeletePlan::Inert,
            ),
            (
                "trash-first",
                serde_json::json!({
                    "id": "b",
                    "status": "Running",
                    "cleanup_defaults": {"delete_worktree": false, "delete_branch": false,
                                         "delete_sandbox": false, "delete_to_trash": true},
                }),
                DeletePlan::Trash,
            ),
            (
                "already trashed",
                serde_json::json!({
                    "id": "c",
                    "status": "Running",
                    "trashed_at": "2026-01-02T03:04:05Z",
                    "cleanup_defaults": {"delete_worktree": false, "delete_branch": false,
                                         "delete_sandbox": false, "delete_to_trash": true},
                }),
                DeletePlan::Permanent,
            ),
            (
                "remote does not trash first",
                serde_json::json!({
                    "id": "d",
                    "status": "Running",
                    "cleanup_defaults": {"delete_worktree": false, "delete_branch": false,
                                         "delete_sandbox": false, "delete_to_trash": false},
                }),
                DeletePlan::Permanent,
            ),
        ] {
            assert_eq!(plan_for(&row(value)), expected, "{name}");
        }
    }

    #[test]
    fn the_dialog_is_seeded_from_the_wire_defaults_and_names_the_machine() {
        let wire = row(serde_json::json!({
            "id": "r1",
            "title": "refactor",
            "status": "Running",
            "project_path": "/Users/remote/app-wt",
            "branch": "feature/x",
            "has_cleanable_worktree": true,
            "is_sandboxed": true,
            "cleanup_defaults": {"delete_worktree": true, "delete_branch": true,
                                 "delete_sandbox": true, "delete_to_trash": false},
        }));
        // No local config is read: the checkboxes come from the row alone, and
        // this machine's `default` profile cleans up nothing by default.
        let options = delete_dialog("mini", &wire).options().clone();
        assert!(options.delete_worktree);
        assert!(options.delete_branch);
        assert!(options.delete_sandbox);
        assert!(!options.force_delete);
        assert_eq!(dialog_title("mini", &wire), "refactor on mini");
    }

    #[test]
    fn a_wire_default_for_an_absent_artifact_cannot_submit_as_ticked() {
        // The remote's profile cleans up worktrees and sandboxes, but this row
        // has neither, so the dialog must not offer to remove them.
        let dialog = delete_dialog(
            "mini",
            &row(serde_json::json!({
                "id": "r2",
                "title": "notes",
                "status": "Running",
                "cleanup_defaults": {"delete_worktree": true, "delete_branch": true,
                                     "delete_sandbox": true, "delete_to_trash": false},
            })),
        );
        let options = dialog.options();
        assert!(!options.delete_worktree);
        assert!(!options.delete_branch);
        assert!(!options.delete_sandbox);
    }

    #[test]
    fn a_failed_delete_is_named_by_its_remote_and_described_from_its_status() {
        let status = |status, code| DaemonClientError::Status {
            status,
            code,
            body: "server text".into(),
            truncated: false,
        };
        for (error, expected) in [
            (status(StatusCode::NOT_FOUND, None), "already gone there"),
            (
                status(StatusCode::SERVICE_UNAVAILABLE, None),
                "runtime is not ready",
            ),
            (
                status(
                    StatusCode::FORBIDDEN,
                    Some(crate::daemon::ApiErrorCode::ReadOnly),
                ),
                "read-only",
            ),
            (status(StatusCode::BAD_GATEWAY, None), "HTTP 502"),
        ] {
            let message = delete_failure_message("mini", &error);
            assert!(message.starts_with("mini: "), "{message}");
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("server text"), "{message}");
        }
    }

    #[test]
    fn a_purge_the_daemon_did_not_carry_out_reads_as_a_failure() {
        let done = |result| match result {
            DeleteResult::Done(message) => message,
            DeleteResult::Failed(message) => panic!("expected done: {message}"),
        };
        let failed = |result| match result {
            DeleteResult::Failed(message) => message,
            DeleteResult::Done(message) => panic!("expected failure: {message}"),
        };
        assert_eq!(
            done(purged_result(
                "mini",
                PurgeOutcome::Deleted {
                    messages: Vec::new(),
                    cleanup_errors: Vec::new(),
                },
            )),
            "Deleted on mini"
        );
        let kept = failed(purged_result(
            "mini",
            PurgeOutcome::Kept {
                messages: vec!["worktree has uncommitted changes".into()],
                teardown_started: false,
            },
        ));
        assert!(kept.starts_with("mini kept the session: "), "{kept}");
        assert!(kept.contains("uncommitted"), "{kept}");
    }
}
