//! Shared session-domain service handle.
//!
//! Holds the narrow set of daemon state the session create/turn paths need
//! (live instances, ACP supervisor, storage file-watch, per-instance locks,
//! telemetry counter), so those paths can be driven by callers that do not
//! hold the HTTP `AppState`: today the HTTP handlers, next the plugin host
//! RPCs (#2897). `AppState` constructs one and keeps cloned handles to the
//! same underlying state, so both views stay consistent; neither owns the
//! other, which avoids an `AppState`/`PluginHost` reference cycle.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::session::Instance;

pub struct SessionService {
    /// Live in-memory session list, shared with `AppState.instances`.
    pub instances: Arc<RwLock<Vec<Instance>>>,
    /// Per-instance mutation locks, shared with `AppState.instance_locks`.
    pub instance_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Storage change-notification service, shared with `AppState.file_watch`.
    pub file_watch: Arc<crate::file_watch::FileWatchService>,
    /// Opt-in telemetry create counter, shared with
    /// `AppState.telemetry_session_creates`.
    pub telemetry_session_creates: Arc<std::sync::atomic::AtomicU32>,
    /// Owns the per-session ACP agent subprocesses, shared with
    /// `AppState.acp_supervisor`.
    #[cfg(feature = "serve")]
    pub acp_supervisor:
        Arc<crate::acp::supervisor::Supervisor<crate::acp::supervisor::ChannelSink>>,
}

impl SessionService {
    /// Same lazy per-instance mutex registry as `AppState::instance_lock`;
    /// both operate on the shared map, so a lock taken through either handle
    /// excludes the other.
    pub async fn instance_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        {
            let guard = self.instance_locks.read().await;
            if let Some(lock) = guard.get(id) {
                return lock.clone();
            }
        }
        let mut guard = self.instance_locks.write().await;
        guard
            .entry(id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}
