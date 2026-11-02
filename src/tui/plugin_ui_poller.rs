//! Background fetch for the daemon-owned plugin UI snapshot.

use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use crate::acp::client::{DiscoveryError, HttpClient, HttpError, ManagerError};
use crate::plugin::ui_state::UiSnapshot;
use crate::tui::worker::Worker;

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
struct PluginUiUpdate {
    status: PluginUiFetch,
}

#[derive(Default)]
enum PluginUiFetch {
    /// A freshly fetched snapshot to apply.
    Snapshot(UiSnapshot),
    /// A transient failure: keep the last good snapshot and retry next tick.
    #[default]
    Failed,
    /// The daemon is genuinely gone; clear the snapshot rather than show stale
    /// badges once it stops responding.
    NoDaemon,
    /// The daemon rejected the bearer token; stop polling so the 3s loop does
    /// not turn into repeated IP-wide token lockouts.
    Unauthorized,
}

impl PluginUiUpdate {
    fn snapshot(snapshot: UiSnapshot) -> Self {
        Self {
            status: PluginUiFetch::Snapshot(snapshot),
        }
    }
    fn failed() -> Self {
        Self {
            status: PluginUiFetch::Failed,
        }
    }
    fn no_daemon() -> Self {
        Self {
            status: PluginUiFetch::NoDaemon,
        }
    }
    fn unauthorized() -> Self {
        Self {
            status: PluginUiFetch::Unauthorized,
        }
    }
}

async fn fetch_plugin_ui() -> PluginUiUpdate {
    let endpoint = match crate::acp::client::locate_daemon().await {
        Ok(endpoint) => endpoint,
        Err(ManagerError::EnvOverrideUnauthorized) => {
            tracing::debug!(
                target: "tui.plugin_ui",
                "plugin UI poll stopped: daemon auth rejected"
            );
            return PluginUiUpdate::unauthorized();
        }
        Err(ManagerError::NoDaemonRunning(DiscoveryError::NoLocalDaemon)) => {
            tracing::trace!(target: "tui.plugin_ui", "no daemon for plugin UI");
            return PluginUiUpdate::no_daemon();
        }
        Err(error) => {
            tracing::trace!(target: "tui.plugin_ui", error = %error, "daemon lookup failed");
            return PluginUiUpdate::failed();
        }
    };
    let client = match HttpClient::new(endpoint) {
        Ok(client) => client,
        Err(HttpError::Unauthorized) => {
            tracing::debug!(
                target: "tui.plugin_ui",
                "plugin UI poll stopped: daemon auth rejected"
            );
            return PluginUiUpdate::unauthorized();
        }
        Err(error) => {
            tracing::debug!(target: "tui.plugin_ui", error = %error, "plugin UI client failed");
            return PluginUiUpdate::failed();
        }
    };
    match client.plugin_ui_state().await {
        Ok(snapshot) => PluginUiUpdate::snapshot(snapshot),
        Err(HttpError::Unauthorized) => {
            tracing::debug!(
                target: "tui.plugin_ui",
                "plugin UI poll stopped: daemon auth rejected"
            );
            PluginUiUpdate::unauthorized()
        }
        Err(error) => {
            tracing::debug!(target: "tui.plugin_ui", error = %error, "plugin UI fetch failed");
            PluginUiUpdate::failed()
        }
    }
}

struct PluginUiPoller {
    worker: Worker<(), PluginUiUpdate>,
}

impl PluginUiPoller {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();
        if let Err(error) = &runtime {
            tracing::warn!(target: "tui.plugin_ui", error = %error, "plugin UI runtime build failed");
        }
        Self {
            worker: Worker::spawn("aoe-plugin-ui", move |()| match runtime.as_ref() {
                Ok(runtime) => runtime.block_on(fetch_plugin_ui()),
                Err(_) => PluginUiUpdate::default(),
            }),
        }
    }

    fn request_refresh(&self) {
        self.worker.request(());
    }
}

/// Ephemeral view of the active daemon's plugin contributions. The poller
/// clears it when the daemon is genuinely gone, keeps the last good snapshot on
/// transient failures, and stops on a rejected bearer token. The worker thread
/// and its runtime spawn lazily on the first refresh, so a `HomeView` with no
/// sessions (and any test fixture) never spawns one.
pub(crate) struct PluginUiState {
    pub(crate) snapshot: UiSnapshot,
    poller: Option<PluginUiPoller>,
    pending: bool,
    stopped: bool,
    last_refresh: Instant,
}

impl PluginUiState {
    pub(crate) fn request_refresh(&mut self, has_sessions: bool) {
        if self.stopped
            || !has_sessions
            || self.pending
            || self.last_refresh.elapsed() < REFRESH_INTERVAL
        {
            return;
        }
        self.poller
            .get_or_insert_with(PluginUiPoller::new)
            .request_refresh();
        self.pending = true;
        self.last_refresh = Instant::now();
    }

    pub(crate) fn apply_pending(&mut self) -> bool {
        let Some(poller) = self.poller.as_ref() else {
            return false;
        };
        match poller.worker.try_recv() {
            Ok(update) => {
                self.pending = false;
                match update.status {
                    PluginUiFetch::Snapshot(snapshot) => {
                        let changed = self.snapshot.entries != snapshot.entries;
                        self.snapshot = snapshot;
                        changed
                    }
                    // Keep the last good snapshot so one bad poll does not blank
                    // every row badge or force a redraw.
                    PluginUiFetch::Failed => false,
                    // The daemon is genuinely gone; drop row badges rather than
                    // keep stale ones for a dead local daemon.
                    PluginUiFetch::NoDaemon => {
                        let had_entries = !self.snapshot.entries.is_empty();
                        self.snapshot = UiSnapshot::default();
                        had_entries
                    }
                    // A rejected bearer token cannot recover here; stop instead
                    // of hammering the auth path every 3 seconds.
                    PluginUiFetch::Unauthorized => {
                        self.stopped = true;
                        false
                    }
                }
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                tracing::error!(target: "tui.plugin_ui", "plugin UI worker gone; respawning");
                self.snapshot = UiSnapshot::default();
                self.poller = Some(PluginUiPoller::new());
                self.stopped = false;
                self.pending = false;
                true
            }
        }
    }
}

impl Default for PluginUiState {
    fn default() -> Self {
        Self {
            snapshot: UiSnapshot::default(),
            poller: None,
            pending: false,
            stopped: false,
            last_refresh: Instant::now() - REFRESH_INTERVAL,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::time::Instant;

    use super::{PluginUiPoller, PluginUiState, PluginUiUpdate};
    use crate::plugin::ui_state::{UiEntry, UiSnapshot};
    use crate::tui::worker::Worker;
    use aoe_plugin_api::UiSlot;

    fn entry(text: &str) -> UiEntry {
        UiEntry {
            plugin_id: "test.plugin".into(),
            slot: UiSlot::RowBadge,
            id: "status".into(),
            session_id: Some("session-1".into()),
            payload: serde_json::json!({"text": text}),
        }
    }

    fn snapshot(text: &str, revision: u64) -> UiSnapshot {
        let mut snapshot = UiSnapshot::default();
        snapshot.entries.push(entry(text));
        snapshot.revisions.insert(
            "test.plugin".into(),
            BTreeMap::from([("session-1".into(), revision)]),
        );
        snapshot
    }

    fn state(current: UiSnapshot, update: PluginUiUpdate) -> PluginUiState {
        PluginUiState {
            snapshot: current,
            poller: Some(PluginUiPoller {
                worker: Worker::seeded_for_test("aoe-plugin-ui-poller-test", update),
            }),
            pending: true,
            stopped: false,
            last_refresh: Instant::now(),
        }
    }

    #[test]
    fn apply_pending_redraws_only_for_entry_changes() {
        let cases = [
            ("changed entries", "old", "new", 1, 2, true),
            ("revision only", "same", "same", 1, 2, false),
        ];

        for (name, current_text, update_text, current_revision, update_revision, changed) in cases {
            let mut state = state(
                snapshot(current_text, current_revision),
                PluginUiUpdate::snapshot(snapshot(update_text, update_revision)),
            );
            let expected = snapshot(update_text, update_revision);

            assert_eq!(state.apply_pending(), changed, "{name}");
            assert_eq!(state.snapshot.entries, expected.entries, "{name}");
            assert_eq!(state.snapshot.revisions, expected.revisions, "{name}");
            assert!(!state.pending, "{name}");
        }
    }

    #[test]
    fn failed_fetch_keeps_last_good_snapshot() {
        let current = snapshot("current", 1);
        let mut state = state(current.clone(), PluginUiUpdate::failed());

        assert!(!state.apply_pending());
        assert_eq!(state.snapshot.entries, current.entries);
        assert_eq!(state.snapshot.revisions, current.revisions);
        assert!(!state.pending);
        assert!(!state.stopped);
    }

    #[test]
    fn no_daemon_clears_snapshot_and_requests_redraw() {
        let current = snapshot("stale", 1);
        let mut state = state(current, PluginUiUpdate::no_daemon());

        assert!(state.apply_pending());
        assert!(state.snapshot.entries.is_empty());
        assert!(state.snapshot.revisions.is_empty());
        assert!(!state.pending);
        assert!(!state.stopped);
    }

    #[test]
    fn no_daemon_with_empty_snapshot_skips_redraw() {
        let mut state = state(UiSnapshot::default(), PluginUiUpdate::no_daemon());

        assert!(!state.apply_pending());
        assert!(!state.pending);
    }

    #[test]
    fn unauthorized_fetch_stops_polling_and_keeps_snapshot() {
        let current = snapshot("current", 1);
        let mut state = state(current.clone(), PluginUiUpdate::unauthorized());

        assert!(!state.apply_pending());
        assert!(state.stopped);
        assert_eq!(state.snapshot.entries, current.entries);
        assert!(!state.pending);
    }

    #[test]
    fn stopped_state_ignores_further_refresh_requests() {
        let mut state = state(snapshot("current", 1), PluginUiUpdate::failed());
        state.stopped = true;
        state.pending = false;

        state.request_refresh(true);
        assert!(!state.pending);
    }
}
