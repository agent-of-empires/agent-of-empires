//! Apply a state change (archive, snooze, unread, stop, restart) to a session
//! on a remote daemon.
//!
//! Runs on a worker: archiving tears down panes on the other machine and a
//! restart relaunches an agent there, neither of which may stall the TUI.
//!
//! One worker serves every mutation rather than one worker per action. The
//! daemon already models these as a single `SessionMutation` enum, so the
//! client that drives it across a network should not fan back out into a
//! module per verb.

use std::sync::mpsc::TryRecvError;

use reqwest::StatusCode;

use crate::daemon::{
    DaemonClient, DaemonClientError, SessionMutation, SessionResponse, UpdateArchiveBody,
    UpdateSnoozeBody, UpdateUnreadBody,
};
use crate::session::Status;
use crate::tui::worker::Worker;

/// What a remote row's state keys offer, decided from the row alone: this
/// machine cannot resolve the other daemon's config.
///
/// A row mid-lifecycle has no settled state to toggle, matching
/// [`crate::tui::remote_rename::can_rename`].
pub(crate) fn can_mutate(row: &SessionResponse) -> bool {
    !matches!(
        Status::from_api_str(&row.status),
        Some(Status::Creating) | Some(Status::Deleting)
    )
}

/// Whether a remote row is already down, so the caller offers restart rather
/// than stop. `None` for a status this build does not know, or one mid
/// transition, where neither verb is honest.
pub(crate) fn is_down(row: &SessionResponse) -> Option<bool> {
    match Status::from_api_str(&row.status)? {
        Status::Stopped | Status::Error => Some(true),
        Status::Running | Status::Waiting | Status::Idle => Some(false),
        Status::Starting | Status::Deleting | Status::Creating | Status::Unknown => None,
    }
}

/// The mutation a state toggle sends, and the past-tense verb its success
/// message uses. Built from the row so the label and the request agree.
pub(crate) fn archive(row: &SessionResponse) -> (SessionMutation, &'static str) {
    let archived = row.archived_at.is_none();
    (
        SessionMutation::Archive(UpdateArchiveBody {
            archived,
            kill_pane: true,
        }),
        if archived { "Archived" } else { "Unarchived" },
    )
}

/// Unsnooze, for a row that reports a snooze. Snoozing asks for a duration
/// first, the way a local row does, so it arrives as [`snooze_for`].
pub(crate) fn unsnooze() -> (SessionMutation, &'static str) {
    (
        SessionMutation::Snooze(UpdateSnoozeBody { minutes: None }),
        "Unsnoozed",
    )
}

pub(crate) fn snooze_for(minutes: u32) -> (SessionMutation, &'static str) {
    (
        SessionMutation::Snooze(UpdateSnoozeBody {
            minutes: Some(minutes),
        }),
        "Snoozed",
    )
}

/// Whether the row reports a snooze, so the caller knows which of the two to
/// send and how to label the entry.
pub(crate) fn is_snoozed(row: &SessionResponse) -> bool {
    row.snoozed_until.is_some()
}

pub(crate) fn unread(row: &SessionResponse) -> (SessionMutation, &'static str) {
    let unread = !row.unread;
    (
        SessionMutation::Unread(UpdateUnreadBody { unread }),
        if unread {
            "Marked unread"
        } else {
            "Marked read"
        },
    )
}

pub(crate) struct MutateRequest {
    pub remote: String,
    pub client: DaemonClient,
    pub session_id: String,
    pub mutation: SessionMutation,
    /// Past-tense verb for the success line, e.g. `Archived`.
    pub verb: &'static str,
    /// Row title, so the message names what changed rather than an id.
    pub title: String,
}

/// What one remote mutation produced. Both messages name the remote, so the
/// caller shows them as they are.
pub(crate) enum MutateResult {
    Done(String),
    Failed(String),
}

pub struct RemoteMutate {
    worker: Worker<MutateRequest, MutateResult>,
}

impl RemoteMutate {
    pub fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        Self {
            worker: Worker::spawn(
                "aoe-remote-mutate",
                move |request: MutateRequest| match runtime.as_ref() {
                    Ok(rt) => rt.block_on(run_mutation(&request)),
                    Err(e) => MutateResult::Failed(format!("{}: no runtime: {e}", request.remote)),
                },
            ),
        }
    }

    pub(crate) fn request(&self, request: MutateRequest) {
        self.worker.request(request);
    }

    pub(crate) fn try_recv(&self) -> Result<MutateResult, TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for RemoteMutate {
    fn default() -> Self {
        Self::new()
    }
}

async fn run_mutation(request: &MutateRequest) -> MutateResult {
    let remote = request.remote.as_str();
    match request
        .client
        .mutate_session_unpinned(&request.session_id, &request.mutation)
        .await
    {
        Ok(()) => MutateResult::Done(format!("{} '{}' on {remote}", request.verb, request.title)),
        Err(e) => MutateResult::Failed(failure_message(remote, request.verb, &e)),
    }
}

/// Why a mutation failed, from the status and the `aoe-error-code` header: the
/// daemon's error body is never read.
fn failure_message(remote: &str, verb: &str, error: &DaemonClientError) -> String {
    let detail = match error {
        DaemonClientError::Status {
            status: StatusCode::NOT_FOUND,
            code: None,
            ..
        } => "the session is already gone there (HTTP 404)".to_string(),
        DaemonClientError::Status {
            status: StatusCode::CONFLICT,
            code: None,
            ..
        } => "its state moved under the request (HTTP 409); the next poll shows where it landed"
            .to_string(),
        other => other.summary(),
    };
    format!("{remote}: {} failed, {detail}", verb.to_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(value: serde_json::Value) -> SessionResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn a_row_mid_lifecycle_has_no_settled_state_to_toggle() {
        for (status, expected) in [
            ("Creating", false),
            ("Deleting", false),
            ("Running", true),
            ("Stopped", true),
        ] {
            let row = row(serde_json::json!({"id": "a", "status": status}));
            assert_eq!(can_mutate(&row), expected, "{status}");
        }
    }

    /// Each toggle reads the row's own state, so the request and the label the
    /// menu drew from the same row cannot disagree.
    #[test]
    fn a_toggle_sends_the_opposite_of_what_the_row_reports() {
        let idle = row(serde_json::json!({"id": "a", "status": "Idle"}));
        let archived = row(
            serde_json::json!({"id": "a", "status": "Archived", "archived_at": "2026-09-21T00:00:00Z"}),
        );
        assert!(matches!(
            archive(&idle),
            (
                SessionMutation::Archive(UpdateArchiveBody { archived: true, .. }),
                "Archived"
            )
        ));
        assert!(matches!(
            archive(&archived),
            (
                SessionMutation::Archive(UpdateArchiveBody {
                    archived: false,
                    ..
                }),
                "Unarchived"
            )
        ));

        let snoozed = row(
            serde_json::json!({"id": "a", "status": "Idle", "snoozed_until": "2026-09-21T00:00:00Z"}),
        );
        assert!(!is_snoozed(&idle));
        assert!(is_snoozed(&snoozed));
        assert!(matches!(
            snooze_for(30),
            (
                SessionMutation::Snooze(UpdateSnoozeBody { minutes: Some(30) }),
                "Snoozed"
            )
        ));
        assert!(matches!(
            unsnooze(),
            (
                SessionMutation::Snooze(UpdateSnoozeBody { minutes: None }),
                "Unsnoozed"
            )
        ));

        let unread_row = row(serde_json::json!({"id": "a", "status": "Idle", "unread": true}));
        assert!(matches!(
            unread(&idle),
            (
                SessionMutation::Unread(UpdateUnreadBody { unread: true }),
                "Marked unread"
            )
        ));
        assert!(matches!(
            unread(&unread_row),
            (
                SessionMutation::Unread(UpdateUnreadBody { unread: false }),
                "Marked read"
            )
        ));
    }

    /// A row in transition offers neither verb: "Stop" on a starting session
    /// and "Restart" on a deleting one both read as promises the next poll
    /// would contradict.
    #[test]
    fn only_a_settled_row_is_offered_stop_or_restart() {
        for (status, expected) in [
            ("Running", Some(false)),
            ("Waiting", Some(false)),
            ("Idle", Some(false)),
            ("Stopped", Some(true)),
            ("Error", Some(true)),
            ("Starting", None),
            ("Creating", None),
            ("no-such-status", None),
        ] {
            let row = row(serde_json::json!({"id": "a", "status": status}));
            assert_eq!(is_down(&row), expected, "{status}");
        }
    }

    #[test]
    fn a_failed_mutation_is_named_by_its_remote_and_never_quotes_the_body() {
        let status = |status, code| DaemonClientError::Status {
            status,
            code,
            body: "server text".into(),
            truncated: false,
        };
        for (error, expected) in [
            (status(StatusCode::NOT_FOUND, None), "already gone there"),
            (
                status(StatusCode::CONFLICT, None),
                "moved under the request",
            ),
            (
                status(
                    StatusCode::FORBIDDEN,
                    Some(crate::daemon::ApiErrorCode::ReadOnly),
                ),
                "read-only",
            ),
        ] {
            let message = failure_message("mini", "Archived", &error);
            assert!(message.starts_with("mini: archived failed, "), "{message}");
            assert!(message.contains(expected), "{message}");
            assert!(!message.contains("server text"), "{message}");
        }
    }
}
