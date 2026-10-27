//! Background fetch for the daemon-owned plugin UI snapshot.

use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use crate::acp::client::HttpClient;
use crate::plugin::ui_state::UiSnapshot;
use crate::tui::worker::Worker;

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Default)]
struct PluginUiUpdate {
    snapshot: UiSnapshot,
}

async fn fetch_plugin_ui() -> PluginUiUpdate {
    let endpoint = match crate::acp::client::locate_daemon().await {
        Ok(endpoint) => endpoint,
        Err(error) => {
            tracing::trace!(target: "tui.plugin_ui", "no daemon for plugin UI: {error}");
            return PluginUiUpdate::default();
        }
    };
    let client = match HttpClient::new(endpoint) {
        Ok(client) => client,
        Err(error) => {
            tracing::debug!(target: "tui.plugin_ui", "plugin UI client: {error}");
            return PluginUiUpdate::default();
        }
    };
    let snapshot = client.plugin_ui_state().await;
    if let Err(error) = &snapshot {
        tracing::debug!(target: "tui.plugin_ui", "plugin UI fetch: {error}");
    }
    PluginUiUpdate {
        snapshot: snapshot.unwrap_or_default(),
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
            tracing::warn!(target: "tui.plugin_ui", "plugin UI runtime build failed: {error}");
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

/// Ephemeral view of the active daemon's plugin contributions. Not persisted:
/// a daemon disconnect produces an empty snapshot rather than stale row badges.
pub(crate) struct PluginUiState {
    pub(crate) snapshot: UiSnapshot,
    poller: PluginUiPoller,
    pending: bool,
    last_refresh: Instant,
}

impl PluginUiState {
    pub(crate) fn request_refresh(&mut self, has_sessions: bool) {
        if !has_sessions || self.pending || self.last_refresh.elapsed() < REFRESH_INTERVAL {
            return;
        }
        self.poller.request_refresh();
        self.pending = true;
        self.last_refresh = Instant::now();
    }

    pub(crate) fn apply_pending(&mut self) -> bool {
        match self.poller.worker.try_recv() {
            Ok(update) => {
                let changed = self.snapshot.entries != update.snapshot.entries;
                self.snapshot = update.snapshot;
                self.pending = false;
                changed
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                tracing::error!(target: "tui.plugin_ui", "plugin UI worker gone; respawning");
                self.snapshot = UiSnapshot::default();
                self.poller = PluginUiPoller::new();
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
            poller: PluginUiPoller::new(),
            pending: false,
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

    fn state(current: UiSnapshot, update: UiSnapshot) -> PluginUiState {
        PluginUiState {
            snapshot: current,
            poller: PluginUiPoller {
                worker: Worker::seeded_for_test(
                    "aoe-plugin-ui-poller-test",
                    PluginUiUpdate { snapshot: update },
                ),
            },
            pending: true,
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
                snapshot(update_text, update_revision),
            );
            let expected = snapshot(update_text, update_revision);

            assert_eq!(state.apply_pending(), changed, "{name}");
            assert_eq!(state.snapshot.entries, expected.entries, "{name}");
            assert_eq!(state.snapshot.revisions, expected.revisions, "{name}");
            assert!(!state.pending, "{name}");
        }
    }
}
