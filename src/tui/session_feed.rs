//! The sidebar's subscription to the daemon's session list.
//!
//! `GET /api/sessions` is the read the web dashboard polls, fetched here
//! through the shared [`crate::daemon::DaemonClient`] so both surfaces decode
//! one wire contract. Each tick hands the home view either the daemon's rows
//! or an unavailable report, and the home view projects the daemon-owned
//! fields onto its rows (see `HomeView::apply_session_feed`).
//!
//! The daemon-owned set is narrow while the TUI still writes durable session
//! state: today it is the runtime status of structured (ACP) rows, which have
//! no tmux pane and whose status the daemon never persists (see
//! `apply_acp_overlay_inplace`). Terminal rows stay with the local tmux
//! poller, which the daemon does not own yet.
//!
//! No daemon reachable, or `session.daemon_sidebar` off, means the local
//! session store serves the sidebar alone and daemon-owned state keeps its
//! last value. That is not an error: a structured session cannot be running
//! without a daemon (`require_daemon` refuses to auto-spawn), so there is no
//! live status to miss.

use std::sync::mpsc::TryRecvError;

use crate::daemon::SessionResponse;
use crate::session::{Status, View};
use crate::tui::worker::Worker;

/// What one fetch of the session list produced.
#[derive(Debug)]
pub(crate) enum SessionFeedResult {
    /// The daemon answered; these are its rows, unfiltered.
    Snapshot(Vec<SessionResponse>),
    /// No daemon answered. The reason is for the transition log only.
    Unavailable(String),
}

/// Where the sidebar's daemon-owned state currently comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SidebarSource {
    /// The local session store only; daemon-owned state keeps its last value.
    Storage,
    /// A reachable daemon; daemon-owned state follows `/api/sessions`.
    Daemon,
}

/// One structured row's status as the daemon sees it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DaemonStatusUpdate {
    pub id: String,
    pub status: Status,
    pub last_error: Option<String>,
    pub last_accessed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub idle_entered_at: Option<chrono::DateTime<chrono::Utc>>,
    pub pending_approvals: Vec<crate::daemon::PendingApproval>,
}

/// Project the daemon's rows onto the structured sessions the TUI cares
/// about. Pure so the wire-shape handling is testable without a daemon.
///
/// Terminal rows are dropped: the tmux poller owns those, and letting the
/// daemon's copy through would give them two producers racing on
/// alternating cycles. An unparseable `status` is dropped rather than
/// coerced, so a newer daemon variant leaves the row alone instead of
/// forcing it to a wrong value.
pub(crate) fn structured_updates(rows: &[SessionResponse]) -> Vec<DaemonStatusUpdate> {
    rows.iter()
        .filter(|row| row.view == View::Structured)
        .filter_map(|row| {
            let status = Status::from_api_str(&row.status)?;
            Some(DaemonStatusUpdate {
                id: row.id.clone(),
                status,
                last_error: row.last_error.clone(),
                last_accessed_at: parse_ts(row.last_accessed_at.as_deref()),
                idle_entered_at: parse_ts(row.idle_entered_at.as_deref()),
                pending_approvals: row.pending_approvals.clone(),
            })
        })
        .collect()
}

fn parse_ts(raw: Option<&str>) -> Option<chrono::DateTime<chrono::Utc>> {
    let raw = raw?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|t| t.with_timezone(&chrono::Utc))
}

async fn fetch_sessions() -> SessionFeedResult {
    // `require_daemon` is the same resolver `open_structured_view` uses, so
    // this feed and the view it feeds can never disagree about which daemon
    // they are talking to, and neither spawns one as a side effect.
    //
    // Its `AOE_DAEMON_URL` branch is unreachable from here: `tui::run` swaps
    // the whole app over to `remote_home::run_standalone` when that variable
    // is set (`src/tui/mod.rs`), so the local home view this feed belongs to
    // only ever runs against a local daemon.
    let endpoint = match crate::acp::client::require_daemon().await {
        Ok(endpoint) => endpoint,
        Err(e) => {
            tracing::trace!(target: "tui.session_feed", "no daemon: {e}");
            return SessionFeedResult::Unavailable(e.to_string());
        }
    };
    let client = match endpoint.daemon_client() {
        Ok(client) => client,
        Err(e) => {
            tracing::debug!(target: "tui.session_feed", "daemon client: {e}");
            return SessionFeedResult::Unavailable(e.to_string());
        }
    };
    match client.list_sessions(None).await {
        Ok(envelope) => SessionFeedResult::Snapshot(envelope.sessions),
        Err(e) => {
            tracing::debug!(target: "tui.session_feed", "session list fetch: {e}");
            SessionFeedResult::Unavailable(e.to_string())
        }
    }
}

/// Background thread that reads the session list from the daemon.
pub struct SessionFeed {
    worker: Worker<(), SessionFeedResult>,
}

impl SessionFeed {
    pub fn new() -> Self {
        // One current-thread runtime for the worker's lifetime. Building it
        // per request would pay setup on every tick, and the TUI's own
        // runtime is not reachable from this thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Err(e) = &runtime {
            tracing::warn!(
                target: "tui.session_feed",
                "runtime build failed; daemon-owned state stays at its last value: {e}"
            );
        }
        Self {
            worker: Worker::spawn("aoe-session-feed", move |()| match runtime.as_ref() {
                Ok(rt) => rt.block_on(fetch_sessions()),
                Err(e) => SessionFeedResult::Unavailable(format!("no runtime: {e}")),
            }),
        }
    }

    /// Request a fetch (non-blocking).
    pub fn request_refresh(&self) {
        self.worker.request(());
    }

    /// Try to receive a result without blocking. Surfaces `Disconnected` so
    /// the caller can respawn: swallowing it would leave the in-flight flag
    /// set forever and freeze every daemon-owned row.
    pub(crate) fn try_recv(&self) -> Result<SessionFeedResult, TryRecvError> {
        self.worker.try_recv()
    }

    /// Feed with one pre-seeded result and no daemon behind it.
    #[cfg(test)]
    pub(crate) fn seeded_for_test(result: SessionFeedResult) -> Self {
        Self {
            worker: Worker::seeded_for_test("aoe-session-feed-test", result),
        }
    }
}

impl Default for SessionFeed {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: &str, view: View) -> SessionResponse {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "status": status,
            "view": view,
        }))
        .unwrap()
    }

    #[test]
    fn structured_updates_keeps_only_structured_rows() {
        let updates = structured_updates(&[
            row("a", "Running", View::Structured),
            row("b", "Running", View::Terminal),
        ]);
        assert_eq!(updates.len(), 1);
        assert_eq!(updates[0].id, "a");
        assert_eq!(updates[0].status, Status::Running);
    }

    #[test]
    fn structured_updates_drops_unparseable_status() {
        // A newer daemon variant must leave the row alone, not coerce it to
        // a wrong status.
        let updates = structured_updates(&[row("a", "Hibernating", View::Structured)]);
        assert!(updates.is_empty());
    }

    #[test]
    fn structured_updates_parses_every_status_the_daemon_emits() {
        for status in [
            Status::Running,
            Status::Waiting,
            Status::Idle,
            Status::Unknown,
            Status::Stopped,
            Status::Error,
            Status::Starting,
            Status::Deleting,
            Status::Creating,
        ] {
            let wire = format!("{status:?}");
            let updates = structured_updates(&[row("a", &wire, View::Structured)]);
            assert_eq!(
                updates.first().map(|u| u.status),
                Some(status),
                "wire form {wire} must round-trip"
            );
        }
    }

    #[test]
    fn structured_updates_carries_error_and_timestamps() {
        let row: SessionResponse = serde_json::from_value(serde_json::json!({
            "id": "a",
            "status": "Error",
            "last_error": "agent failed to start",
            "last_accessed_at": "2026-07-30T12:00:00Z",
            "view": "structured",
        }))
        .unwrap();
        let updates = structured_updates(&[row]);
        assert_eq!(updates[0].status, Status::Error);
        assert_eq!(
            updates[0].last_error.as_deref(),
            Some("agent failed to start")
        );
        assert!(updates[0].last_accessed_at.is_some());
        assert_eq!(updates[0].idle_entered_at, None);
    }

    #[test]
    fn parse_ts_rejects_garbage() {
        assert!(parse_ts(Some("not a timestamp")).is_none());
        assert!(parse_ts(None).is_none());
        assert!(parse_ts(Some("2026-07-30T12:00:00Z")).is_some());
    }
}
