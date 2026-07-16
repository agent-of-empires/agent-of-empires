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

/// Typed outcome of [`SessionService::send_turn`], split by whether the
/// failure happened before or after the prompt was published into the event
/// stream, so callers can map each stage faithfully (the HTTP handler keeps
/// its exact pre-extraction status codes, and only fires the post-publish
/// smart-rename hook when a publish actually happened).
#[cfg(feature = "serve")]
pub(crate) enum SendTurnError {
    /// Pre-publish: the session vanished (or was triaged) before the resume
    /// snapshot. Nothing was published; the honest answer is "not found",
    /// not a retryable worker_not_ready. See #1748.
    SessionNotFound,
    /// Pre-publish: reserving the resume slot failed (includes
    /// `SupervisorError::CapacityFull`). Nothing was published.
    ResumeFailed(crate::acp::supervisor::SupervisorError),
    /// Post-publish: the respawn kicked by this call did not finish within
    /// `send_prompt`'s wait window (slow sandbox / spawn). The worker is
    /// still coming; retryable. See #1748.
    WorkerNotReady,
    /// Post-publish: the forward to the agent failed.
    Send(crate::acp::supervisor::SupervisorError),
}

impl SessionService {
    /// Deliver a turn to a structured session: resume a dead/dormant worker
    /// if needed, publish the prompt into the event stream, then forward it
    /// to the agent. Extracted from the `acp_prompt` HTTP handler so a
    /// non-HTTP caller (the plugin host, #2897) delivers turns through the
    /// same path; the handler keeps HTTP concerns (read-only gate, wake,
    /// attachment validation, smart-rename, status mapping).
    ///
    /// `woke_idle_dormant` forces the resume trigger even when the worker
    /// looks alive, mirroring the handler's idle-dormant wake (#1689).
    #[cfg(feature = "serve")]
    pub(crate) async fn send_turn(
        self: &Arc<Self>,
        id: &str,
        text: &str,
        attachments: &[crate::acp::event_store::AttachmentBlob],
        woke_idle_dormant: bool,
    ) -> Result<(), SendTurnError> {
        use crate::server::acp_reconciler::ResumeTrigger;
        // Resume a worker that is not currently live. Two cases:
        //   - Idle-dormant wake: the worker was auto-stopped for inactivity
        //     (#1689) and the reconciler will not respawn it until its next
        //     ~2s tick.
        //   - Dead worker: the worker exited for another reason (e.g. the
        //     silent-orphan watchdog escalated a monitor / `/loop` turn) and
        //     is neither dormant nor mid-respawn, so a send would otherwise
        //     404 and force a manual `aoe acp restart`.
        // Either way, reserve the resume slot synchronously and drive a fresh
        // spawn in a detached task NOW so the `send_prompt` below blocks on
        // `wait_for_worker` until the worker is live instead of racing ahead
        // to a 404. The detached task survives the originating request being
        // cancelled on client disconnect. `is_running` is true for a live or
        // mid-respawn worker, so a healthy session never double-spawns. See
        // #1748.
        let needs_resume = woke_idle_dormant || !self.acp_supervisor.is_running(id).await;
        if needs_resume {
            match crate::server::acp_reconciler::trigger_resume_background(self, id).await {
                Ok(ResumeTrigger::NotFound) => return Err(SendTurnError::SessionNotFound),
                Ok(_) => {}
                Err(e) => return Err(SendTurnError::ResumeFailed(e)),
            }
        }
        // Publish the user's prompt into the event stream BEFORE forwarding
        // to the agent so the replay buffer / on-disk store captures it
        // even if the agent forward fails. The frontend treats UserPromptSent
        // as authoritative and dedupes against its own optimistic row.
        self.acp_supervisor
            .publish_user_prompt_with_attachments(id, text.to_string(), attachments)
            .await;
        match self.acp_supervisor.send_prompt(id, text, attachments).await {
            Ok(()) => Ok(()),
            // Intentional override of the canonical UnknownSession 404: the
            // respawn we kicked above did not finish within `send_prompt`'s
            // wait window. See the `WorkerNotReady` variant doc.
            Err(crate::acp::supervisor::SupervisorError::UnknownSession(_)) if needs_resume => {
                Err(SendTurnError::WorkerNotReady)
            }
            Err(e) => Err(SendTurnError::Send(e)),
        }
    }

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
