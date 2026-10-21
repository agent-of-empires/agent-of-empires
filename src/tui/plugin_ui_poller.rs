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
    let endpoint = match crate::acp::client::require_daemon().await {
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
                self.snapshot = update.snapshot;
                self.pending = false;
                true
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
