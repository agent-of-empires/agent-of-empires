//! Structured (ACP) session status, sourced from the `aoe serve` daemon.
//!
//! Structured sessions have no tmux pane, so the tmux/docker probes in
//! [`crate::tui::status_poller`] cannot observe them (they bail on
//! `is_structured()`). The authority is the daemon: `derive_acp_status`
//! maps ACP events onto `Running` / `Waiting` / `Idle` / `Error` and
//! `apply_status_intent` folds them into the daemon's in-memory
//! `state.instances`. That value is deliberately never persisted to
//! `sessions.json` (see the durability contract on
//! `apply_acp_overlay_inplace`), so a TUI reading disk would show a
//! structured row frozen at whatever creation or an explicit start/stop
//! wrote. This poller closes that gap the same way the web dashboard
//! does: by asking `GET /api/sessions`.
//!
//! No daemon reachable means an empty result, not an error state. A
//! structured session cannot be running without one (the event store is
//! opened by `aoe serve`, and `require_daemon` refuses to auto-spawn), so
//! there is no status to report and the row keeps its last value.

use std::sync::mpsc::TryRecvError;

use serde::Deserialize;

use crate::session::{Status, View};
use crate::tui::worker::Worker;

/// One structured row's status as the daemon sees it.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DaemonStatusUpdate {
    pub id: String,
    pub status: Status,
    pub last_error: Option<String>,
    pub last_accessed_at: Option<chrono::DateTime<chrono::Utc>>,
    pub idle_entered_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Subset of `/api/sessions`'s `SessionResponse` this poller reads.
/// `serde` ignores unknown fields, so the daemon can grow the response
/// without breaking an older TUI. Every *optional* field carries
/// `#[serde(default)]` for the same reason in reverse: an older daemon that
/// omits one must not fail the whole parse and blank out every row's status
/// for that tick.
///
/// `id` is deliberately required. It identifies the row, so a row without one
/// is unusable rather than degraded, and `SessionResponse.id` is a plain
/// `String` with no `skip_serializing_if`, so no daemon version can omit it.
/// Defaulting it would trade a loud parse failure for a silently dropped row.
#[derive(Debug, Clone, Deserialize)]
struct DaemonSessionRow {
    id: String,
    #[serde(default)]
    status: String,
    #[serde(default)]
    last_error: Option<String>,
    #[serde(default)]
    last_accessed_at: Option<String>,
    #[serde(default)]
    idle_entered_at: Option<String>,
    #[serde(default)]
    view: View,
}

/// Project the daemon's rows onto the structured sessions the TUI cares
/// about. Pure so the wire-shape handling is testable without a daemon.
///
/// Terminal rows are dropped: the tmux poller owns those, and letting the
/// daemon's copy through would give them two producers racing on
/// alternating cycles. An unparseable `status` is dropped rather than
/// coerced, so a newer daemon variant leaves the row alone instead of
/// forcing it to a wrong value.
fn structured_updates(rows: Vec<DaemonSessionRow>) -> Vec<DaemonStatusUpdate> {
    rows.into_iter()
        .filter(|row| row.view == View::Structured)
        .filter_map(|row| {
            let status = Status::from_api_str(&row.status)?;
            Some(DaemonStatusUpdate {
                id: row.id,
                status,
                last_error: row.last_error,
                last_accessed_at: parse_ts(row.last_accessed_at.as_deref()),
                idle_entered_at: parse_ts(row.idle_entered_at.as_deref()),
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

async fn fetch_structured_statuses() -> Vec<DaemonStatusUpdate> {
    // `require_daemon` is the same resolver `open_structured_view` uses, so
    // this poller and the view it feeds can never disagree about which daemon
    // they are talking to, and neither spawns one as a side effect.
    //
    // Its `AOE_DAEMON_URL` branch is unreachable from here: `tui::run` swaps
    // the whole app over to `remote_home::run_standalone` when that variable
    // is set (`src/tui/mod.rs`), so the local home view this poller belongs to
    // only ever runs against a local daemon. Kept anyway rather than dropping
    // to a bare `discover()`, so that if the remote-home split is ever folded
    // back into one home view, this reads the daemon the user asked for
    // instead of silently reading the local one.
    let endpoint = match crate::acp::client::require_daemon().await {
        Ok(endpoint) => endpoint,
        Err(e) => {
            tracing::trace!(
                target: "tui.daemon_status",
                "no daemon for structured status: {e}"
            );
            return Vec::new();
        }
    };
    let client = match crate::acp::client::HttpClient::new(endpoint) {
        Ok(client) => client,
        Err(e) => {
            tracing::debug!(
                target: "tui.daemon_status",
                "daemon status client: {e}"
            );
            return Vec::new();
        }
    };
    match client.list_sessions::<DaemonSessionRow>().await {
        Ok(rows) => structured_updates(rows),
        Err(e) => {
            tracing::debug!(
                target: "tui.daemon_status",
                "daemon status fetch: {e}"
            );
            Vec::new()
        }
    }
}

/// Background thread that reads structured-session status from the daemon.
pub struct DaemonStatusPoller {
    worker: Worker<(), Vec<DaemonStatusUpdate>>,
}

impl DaemonStatusPoller {
    pub fn new() -> Self {
        // One current-thread runtime for the worker's lifetime. Building it
        // per request would pay setup on every tick, and the TUI's own
        // runtime is not reachable from this thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Err(e) = &runtime {
            tracing::warn!(
                target: "tui.daemon_status",
                "runtime build failed; structured status stays at its last value: {e}"
            );
        }
        Self {
            worker: Worker::spawn("aoe-daemon-status", move |()| match runtime.as_ref() {
                Ok(rt) => rt.block_on(fetch_structured_statuses()),
                Err(_) => Vec::new(),
            }),
        }
    }

    /// Request a fetch (non-blocking).
    pub fn request_refresh(&self) {
        self.worker.request(());
    }

    /// Try to receive results without blocking. Surfaces `Disconnected` so
    /// the caller can respawn: swallowing it would leave the in-flight flag
    /// set forever and freeze every structured row's status, the exact bug
    /// this poller exists to fix.
    pub fn try_recv_updates(&self) -> Result<Vec<DaemonStatusUpdate>, TryRecvError> {
        self.worker.try_recv()
    }
}

impl Default for DaemonStatusPoller {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, status: &str, view: View) -> DaemonSessionRow {
        DaemonSessionRow {
            id: id.to_string(),
            status: status.to_string(),
            last_error: None,
            last_accessed_at: None,
            idle_entered_at: None,
            view,
        }
    }

    #[test]
    fn structured_updates_keeps_only_structured_rows() {
        let updates = structured_updates(vec![
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
        let updates = structured_updates(vec![row("a", "Hibernating", View::Structured)]);
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
            let updates = structured_updates(vec![row("a", &wire, View::Structured)]);
            assert_eq!(
                updates.first().map(|u| u.status),
                Some(status),
                "wire form {wire} must round-trip"
            );
        }
    }

    #[test]
    fn structured_updates_carries_error_and_timestamps() {
        let updates = structured_updates(vec![DaemonSessionRow {
            id: "a".into(),
            status: "Error".into(),
            last_error: Some("agent failed to start".into()),
            last_accessed_at: Some("2026-07-30T12:00:00Z".into()),
            idle_entered_at: None,
            view: View::Structured,
        }]);
        assert_eq!(updates[0].status, Status::Error);
        assert_eq!(
            updates[0].last_error.as_deref(),
            Some("agent failed to start")
        );
        assert!(updates[0].last_accessed_at.is_some());
        assert_eq!(updates[0].idle_entered_at, None);
    }

    #[test]
    fn daemon_row_deserializes_from_a_minimal_response() {
        // An older daemon that omits fields must still parse; a hard failure
        // here would blank out every structured row's status at once.
        let row: DaemonSessionRow = serde_json::from_str(r#"{"id":"a"}"#).unwrap();
        assert_eq!(row.view, View::Terminal);
        assert_eq!(row.status, "");
        assert!(Status::from_api_str(&row.status).is_none());
    }

    #[test]
    fn parse_ts_rejects_garbage() {
        assert!(parse_ts(Some("not a timestamp")).is_none());
        assert!(parse_ts(None).is_none());
        assert!(parse_ts(Some("2026-07-30T12:00:00Z")).is_some());
    }
}
