//! Turn state shared between the notification handler and the command loop,
//! and the `session/update` handler itself.

use crate::acp::agent_profiles::AgentProfile;
use crate::acp::background_agent::{spawn_tailer, TailerStart, TranscriptSource};
use crate::acp::state::Event;
use agent_client_protocol::schema::v1::{SessionId, SessionNotification, SessionUpdate};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::debug;

use crate::acp::acp_client::between_prompt::BetweenPromptTracker;
use crate::acp::acp_client::lifecycle::{
    forward_lifecycle_signals, LifecycleEnvelope, TerminalClaim,
};
use crate::acp::acp_client::rate_limit::rate_limit_rejection_from_meta;
use crate::acp::acp_client::session_identity::SessionIngress;
use crate::acp::acp_client::session_sandbox::SessionSandbox;
use crate::acp::acp_client::tool_context::{
    update_tool_context_cache, ToolCallContextCache, ToolContextCache,
};
use crate::acp::acp_client::transcript_filter::{is_transcript_event, transcript_event_kind};
use crate::acp::acp_client::update_events::{map_update_to_events, AgentMessageDedup};
use crate::acp::acp_client::watchdog::classify_watchdog_notification_signals;

pub(super) fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub(super) struct Shared {
    pub(super) event_tx: mpsc::Sender<Event>,
    /// Admits only the lifecycle-selected native session; everything the
    /// agent sends for another id is refused or buffered here (#3937).
    pub(super) ingress: Arc<SessionIngress>,
    pub(super) session_label: String,
    pub(super) profile: &'static AgentProfile,
    /// Set between a successful `session/load` and the first prompt, while
    /// the adapter replays history the event store already holds.
    pub(super) suppress_history_replay: AtomicBool,
    /// Epoch-ms of the last inbound notification, for the resume-idle
    /// watchdog.
    pub(super) last_event_at: AtomicI64,
    /// A lifecycle-bearing notification arrived after attach. Ambient
    /// updates do not prove turn progress.
    pub(super) first_event_after_attach: AtomicBool,
    pub(super) prompt_sent_since_attach: AtomicBool,
    /// Ensures exactly one path publishes each turn's terminal `Stopped`.
    pub(super) terminal_claim: Arc<TerminalClaim>,
    /// A turn adopted mid-flight on resume, which no local prompt owns
    /// (#2899).
    pub(super) adopted_turn_active: AtomicBool,
    /// While set, the per-prompt watchdog owns idle detection.
    pub(super) prompt_in_flight: Arc<AtomicBool>,
    /// Bumped before each prompt so stale lifecycle envelopes are discarded.
    pub(super) prompt_epoch: AtomicU64,
    lifecycle_tx: mpsc::Sender<LifecycleEnvelope>,
    pub(super) between_prompt: BetweenPromptTracker,
    /// Reset epochs (unix seconds) per rejected quota window. Not cleared per
    /// prompt: the adapter only reports a reset on turns with usage, and
    /// stale entries are filtered at read time (#3028, #3152).
    pub(super) rate_limit_rejections: std::sync::Mutex<HashMap<String, i64>>,
    /// A stored-session rejection already emitted its context reset.
    pub(super) context_reset_emitted: AtomicBool,
    /// Scoped to one turn; see `AgentMessageDedup` (#2281).
    pub(super) agent_msg_dedup: std::sync::Mutex<AgentMessageDedup>,
    pub(super) tool_context_cache: ToolContextCache,
    bg_transcript_source: TranscriptSource,
}

impl Shared {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        event_tx: mpsc::Sender<Event>,
        session_label: String,
        profile: &'static AgentProfile,
        lifecycle_tx: mpsc::Sender<LifecycleEnvelope>,
        terminal_claim: Arc<TerminalClaim>,
        prompt_in_flight: Arc<AtomicBool>,
        sandbox: Option<&SessionSandbox>,
        ingress: Arc<SessionIngress>,
    ) -> Self {
        // A sandboxed session's sub-agent transcripts live in the container.
        let bg_transcript_source = match sandbox {
            Some(sandbox) => TranscriptSource::Container {
                runtime: crate::containers::get_container_runtime().base.binary,
                container: sandbox.container_name.clone(),
            },
            None => TranscriptSource::Host,
        };
        Self {
            event_tx,
            ingress,
            session_label,
            profile,
            suppress_history_replay: AtomicBool::new(false),
            last_event_at: AtomicI64::new(now_ms()),
            first_event_after_attach: AtomicBool::new(false),
            prompt_sent_since_attach: AtomicBool::new(false),
            terminal_claim,
            adopted_turn_active: AtomicBool::new(false),
            prompt_in_flight,
            prompt_epoch: AtomicU64::new(0),
            lifecycle_tx,
            between_prompt: BetweenPromptTracker::default(),
            rate_limit_rejections: Default::default(),
            context_reset_emitted: AtomicBool::new(false),
            agent_msg_dedup: Default::default(),
            tool_context_cache: Arc::new(std::sync::Mutex::new(ToolCallContextCache::default())),
            bg_transcript_source,
        }
    }

    pub(super) async fn emit(&self, event: Event) {
        let _ = self.event_tx.send(event).await;
    }

    /// Publish the native session the lifecycle selected and apply the
    /// updates buffered while it was still pending.
    pub(super) async fn commit_session(
        &self,
        id: SessionId,
    ) -> Result<(), agent_client_protocol::Error> {
        let _guard = self.ingress.fence.lock().await;
        let replay = self.ingress.finish(Some(id.clone()))?;
        self.emit(Event::AcpSessionAssigned {
            acp_session_id: id.0.to_string(),
        })
        .await;
        self.apply_replay(replay).await;
        Ok(())
    }

    /// Replayed updates carry no local prompt, so they only rebuild state.
    pub(super) async fn apply_replay(&self, replay: Vec<SessionNotification>) {
        for notification in replay {
            self.handle_notification(notification, false).await;
        }
    }

    /// Re-tail sub-agents a previous daemon launched and left unresolved.
    pub(super) fn resume_background_tailing(&self, launches: Vec<(String, String)>) {
        for (agent_id, output_file) in launches {
            spawn_tailer(
                agent_id,
                output_file,
                self.bg_transcript_source.clone(),
                self.event_tx.clone(),
                self.between_prompt.bg_agents.clone(),
                TailerStart::Resumed,
            );
        }
    }

    pub(super) fn reset_message_dedup(&self) {
        self.agent_msg_dedup
            .lock()
            .expect("agent message dedup mutex poisoned")
            .reset();
    }

    /// `local_prompt_signals` is false for a replayed update committed with
    /// the session: it belongs to no local prompt, so it must not reach the
    /// per-prompt watchdog.
    pub(super) async fn handle_notification(
        &self,
        notification: SessionNotification,
        local_prompt_signals: bool,
    ) {
        self.last_event_at.store(now_ms(), Ordering::Relaxed);
        let suppressing = self.suppress_history_replay.load(Ordering::Relaxed);
        // Drop the adapter's leaked restatement before anything sees it
        // (#2281). Replayed history resets rather than feeds the deduper.
        {
            let mut dedup = self
                .agent_msg_dedup
                .lock()
                .expect("agent message dedup mutex poisoned");
            if suppressing {
                dedup.reset();
            } else if dedup.observe(&notification.update) {
                debug!(
                    target: "acp.protocol",
                    session = %self.session_label,
                    "dropping leaked consolidated agent_message_chunk restatement (#2281)"
                );
                return;
            }
        }
        // One epoch per notification, so its signals belong to the prompt
        // that was current when it arrived.
        let epoch = self.prompt_epoch.load(Ordering::Relaxed);
        let (lifecycle, wakeup) =
            classify_watchdog_notification_signals(&notification.update, self.profile, suppressing);
        if lifecycle.is_some() || wakeup.is_some() {
            self.first_event_after_attach.store(true, Ordering::Relaxed);
        }
        let prompt_active = self.prompt_in_flight.load(Ordering::Relaxed);
        if !prompt_active
            && self.between_prompt.observe(
                lifecycle.as_ref(),
                wakeup.as_ref(),
                now_ms(),
                self.adopted_turn_active.load(Ordering::Relaxed),
                &self.terminal_claim,
            )
        {
            debug!(
                target: "acp.protocol",
                session = %self.session_label,
                "between-prompt watchdog armed for an agent-initiated turn"
            );
        }
        self.capture_rate_limit(&notification.update);
        let update_for_tool_context = notification.update.clone();
        let events = map_update_to_events(notification.update, self.profile);
        // Signals go first: if the event send backpressures, the watchdog must
        // not evaluate without a suppression-bearing signal.
        forward_lifecycle_signals(
            local_prompt_signals && prompt_active && epoch != 0,
            &self.lifecycle_tx,
            epoch,
            lifecycle,
            wakeup,
            &self.session_label,
        )
        .await;
        for event in events {
            if let Event::BackgroundAgentLaunched {
                agent_id,
                output_file,
                ..
            } = &event
            {
                if !suppressing && !output_file.is_empty() {
                    spawn_tailer(
                        agent_id.clone(),
                        output_file.clone(),
                        self.bg_transcript_source.clone(),
                        self.event_tx.clone(),
                        self.between_prompt.bg_agents.clone(),
                        TailerStart::Live,
                    );
                }
            }
            // During replay only transcript events are dropped; ambient state
            // and lifecycle events still pass so pickers stay current.
            if suppressing && is_transcript_event(&event) {
                debug!(
                    target: "acp.protocol",
                    session = %self.session_label,
                    kind = transcript_event_kind(&event),
                    "dropping post-load history-replay event"
                );
                continue;
            }
            update_tool_context_cache(&self.tool_context_cache, &event, &update_for_tool_context);
            if self.event_tx.send(event).await.is_err() {
                break;
            }
        }
    }

    /// Only rejections are retained (#3152); every observation is logged as
    /// the breadcrumb for a wrong reset time.
    fn capture_rate_limit(&self, update: &SessionUpdate) {
        let SessionUpdate::UsageUpdate(u) = update else {
            return;
        };
        let Some(raw) = u.meta.as_ref().and_then(|m| m.get("_claude/rateLimit")) else {
            return;
        };
        let rejection = rate_limit_rejection_from_meta(&u.meta);
        debug!(
            target: "acp.protocol",
            session = %self.session_label,
            observed = %raw,
            retained = rejection.is_some(),
            "observed adapter rate-limit meta"
        );
        if let Some(r) = rejection {
            self.rate_limit_rejections
                .lock()
                .expect("rate-limit capture mutex poisoned")
                .insert(r.window, r.resets_at_secs);
        }
    }
}
