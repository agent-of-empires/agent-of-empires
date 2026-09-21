//! Status and metrics refresh: what the pollers report and how a row's
//! status is applied and hooked.

use super::*;

impl HomeView {
    /// Snapshot of `self.instances` eligible for status polling. In-flight recovery and
    /// restart candidates are excluded: their post-cascade `Instance` arrives through
    /// `apply_recovery_updates` / `apply_restart_results`, and a parallel poll would race
    /// those transitions.
    pub(in crate::tui) fn pollable_instances(&self) -> Vec<Instance> {
        self.instances
            .values()
            .filter(|i| {
                !self.recovery_in_flight.contains(&i.id) && !self.restart_in_flight.contains(&i.id)
            })
            .cloned()
            .collect()
    }

    pub(in crate::tui) fn attached_status_hook_sessions(
        &self,
    ) -> Vec<crate::tui::attached_status_hooks::AttachedStatusHookSession> {
        self.pollable_instances()
            .into_iter()
            .filter_map(|instance| {
                let hook_config = self.status_hook_config_for(&instance);
                hook_config.enabled.then_some(
                    crate::tui::attached_status_hooks::AttachedStatusHookSession {
                        instance,
                        hook_config,
                    },
                )
            })
            .collect()
    }

    /// Request a status refresh in the background (non-blocking).
    /// Call `apply_status_updates` to check for and apply results.
    pub fn request_status_refresh(&mut self) {
        if !self.pending_status_refresh {
            self.status_poller
                .request_refresh(self.pollable_instances());
            self.pending_status_refresh = true;
        }
    }

    /// Apply any pending status updates from the background poller.
    /// Returns true if updates were applied.
    pub fn apply_status_updates(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        match self.status_poller.try_recv_updates() {
            Ok(updates) => {
                for update in updates {
                    self.apply_one_status_update(update);
                }
                self.pending_status_refresh = false;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // The worker thread is gone (a panic in poll_statuses_once). Without a
                // respawn, pending_status_refresh stays set and request_status_refresh
                // never fires again, freezing every session's live status.
                tracing::error!(
                    target: "tui.home",
                    "status poller worker gone; respawning a fresh poller",
                );
                self.reset_status_refresh();
                true
            }
        }
    }

    /// Request a system-health sample in the background while either health
    /// surface is visible. Call `apply_metrics_updates` to pick up the result.
    pub fn request_metrics_refresh(&mut self) {
        let instances = self.pollable_instances();
        let tip_candidate = !self.system_health_tip_earned
            && !self.system_health_discovered
            && instances.len() >= crate::tips::SYSTEM_HEALTH_AGENT_THRESHOLD;
        if (self.show_diagnostics || self.system_health_open || tip_candidate)
            && !self.pending_metrics_refresh
        {
            self.metrics_poller.request_refresh(instances);
            self.pending_metrics_refresh = true;
        }
    }

    /// Apply any pending metrics sample. Returns true if a sample was applied
    /// so the caller can repaint the live readouts.
    pub fn apply_metrics_updates(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        match self.metrics_poller.try_recv_updates() {
            Ok(snapshot) => {
                self.metrics = snapshot;
                self.observe_system_health_tip_load();
                self.pending_metrics_refresh = false;
                true
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // The sampler thread died, so respawn or pending_metrics_refresh stays
                // stuck and the strip freezes.
                tracing::error!(
                    target: "tui.home",
                    "metrics poller worker gone; respawning a fresh poller",
                );
                self.metrics_poller = crate::tui::metrics_poller::MetricsPoller::new();
                self.pending_metrics_refresh = false;
                false
            }
        }
    }

    /// Toggle the diagnostics strip and persist the new state to
    /// `session.show_diagnostics_pane` so it survives restarts.
    pub fn toggle_diagnostics(&mut self) {
        self.show_diagnostics = !self.show_diagnostics;
        let enabled = self.show_diagnostics;
        if let Err(e) = update_config(|config| {
            config.session.show_diagnostics_pane = enabled;
        }) {
            tracing::warn!(
                target: "tui.home",
                "failed to persist show_diagnostics_pane: {e}",
            );
        }
    }

    pub fn open_system_health(&mut self) {
        self.system_health_discovered = true;
        if self.pending_tip_pop.map(|tip| tip.id) == Some("system-health") {
            self.pending_tip_pop = None;
        }
        let already_used = load_config()
            .ok()
            .flatten()
            .is_some_and(|config| config.app_state.used_system_health);
        if !already_used {
            if let Err(error) = update_app_state(|state| state.used_system_health = true) {
                tracing::warn!(target: "tui.home", "failed to persist System Health discovery: {error}");
            } else if let Ok(config) = load_config().map(|config| config.unwrap_or_default()) {
                self.tips_unseen = tips_unseen_count(&config);
            }
        }
        self.system_health_open = true;
        self.system_health_scroll = 0;
        self.diff_view = None;
        self.live_send = None;
        self.request_metrics_refresh();
    }

    pub(super) fn observe_system_health_tip_load(&mut self) {
        if self.system_health_tip_earned || self.system_health_discovered {
            return;
        }
        if self.metrics.counts.agents < crate::tips::SYSTEM_HEALTH_AGENT_THRESHOLD {
            self.system_health_tip_high_samples = 0;
            return;
        }
        self.system_health_tip_high_samples = self.system_health_tip_high_samples.saturating_add(1);
        if self.system_health_tip_high_samples < crate::tips::SYSTEM_HEALTH_SAMPLE_THRESHOLD {
            return;
        }

        self.system_health_tip_earned = true;
        if let Err(error) = update_app_state(|state| state.system_health_tip_earned = true) {
            tracing::warn!(target: "tui.home", "failed to persist System Health tip signal: {error}");
            return;
        }
        let Ok(config) = load_config().map(|config| config.unwrap_or_default()) else {
            return;
        };
        self.tips_unseen = tips_unseen_count(&config);
        if config.session.show_tips
            && !config.app_state.used_system_health
            && !config
                .app_state
                .tips_seen
                .iter()
                .any(|id| id == "system-health")
            && self.pending_tip_pop.is_none()
        {
            self.pending_tip_pop = crate::tips::catalog()
                .iter()
                .find(|tip| tip.id == "system-health");
        }
    }

    /// Request the daemon's session list (non-blocking). Skipped when
    /// `session.daemon_sidebar` is off and when no structured session is loaded, since the
    /// daemon owns nothing on a terminal-only sidebar.
    pub fn request_session_feed_refresh(&mut self) {
        if !self.daemon_sidebar || self.pending_session_feed {
            return;
        }
        if !self.instances.values().any(|i| i.is_structured()) {
            return;
        }
        self.session_feed.request_refresh();
        self.pending_session_feed = true;
    }

    /// Record where daemon-owned sidebar state comes from, logging the transition so a
    /// sidebar stuck on stale structured status is diagnosable from the log alone.
    /// `reason` says why the daemon is not the source and is ignored for `Daemon`.
    pub(super) fn set_sidebar_source(
        &mut self,
        source: crate::tui::session_feed::SidebarSource,
        reason: Option<&str>,
    ) {
        use crate::tui::session_feed::SidebarSource;

        if self.sidebar_source == source {
            return;
        }
        self.sidebar_source = source;
        match source {
            SidebarSource::Daemon => tracing::info!(
                target: "tui.home",
                "sidebar: daemon reachable; structured rows follow /api/sessions",
            ),
            SidebarSource::Storage => tracing::info!(
                target: "tui.home",
                reason = reason.unwrap_or(""),
                "sidebar: local store only; daemon-owned state keeps its last value",
            ),
        }
    }

    /// Whether a daemon-sourced status may be applied to `id`, mirroring the tmux
    /// producer's exclusions. A row mid-restart or mid-recovery-cascade gets its
    /// post-cascade `Instance` from `apply_restart_results` / `apply_recovery_updates`, so
    /// letting the daemon's copy land during that window races them; recovery already
    /// skips structured rows, but both are checked so the producers stay symmetrical.
    ///
    /// Archived and trashed rows are excluded too: `/api/sessions` returns them unfiltered
    /// and the `is_archived()` short-circuit that keeps the tmux producer off a sunk row
    /// lives in `update_status_with_metadata_inner`, which this path never reaches, so a
    /// sunk row would be restamped and re-marked unread. See #3201 / #1868 / #2206.
    ///
    /// The cost: a sunk structured row already in `Status::Error` has no producer able to
    /// clear it, since the daemon is excluded here, the tmux poller bails on structured
    /// rows, and `reload_storage_only` carries `prev.status` forward. It stays visible
    /// because `agent_row_icon` lets `Error` punch through the sunk-row mask on purpose,
    /// and unarchiving is the only way back. The tmux producer has the same property.
    fn daemon_status_applies_to(&self, inst: &Instance) -> bool {
        !self.recovery_in_flight.contains(&inst.id)
            && !self.restart_in_flight.contains(&inst.id)
            && !inst.is_archived()
            && !inst.is_trashed()
    }

    /// Apply a pending session-list result from the daemon. Returns true if
    /// the caller should redraw.
    pub fn apply_session_feed(&mut self) -> bool {
        use crate::tui::session_feed::{self, SessionFeedResult, SidebarSource};
        use std::sync::mpsc::TryRecvError;

        match self.session_feed.try_recv() {
            Ok(result) => {
                self.pending_session_feed = false;
                // A result that was in flight when the setting flipped off is
                // dropped, so "off" means the daemon never drives a row.
                if !self.daemon_sidebar {
                    return false;
                }
                match result {
                    SessionFeedResult::Snapshot(rows) => {
                        self.set_sidebar_source(SidebarSource::Daemon, None);
                        let updates = session_feed::structured_updates(&rows);
                        let applied = !updates.is_empty();
                        for update in updates {
                            self.apply_daemon_status_update(update);
                        }
                        // Drop pending-approval entries for sessions that no longer exist
                        // locally, which the daemon stops listing, so
                        // `apply_daemon_status_update` never revisits them and the entry
                        // would leak for the life of the process.
                        let instances = &self.instances;
                        self.structured_pending_approvals
                            .retain(|id, _| instances.contains_key(id));
                        applied
                    }
                    SessionFeedResult::Unavailable(reason) => {
                        self.set_sidebar_source(SidebarSource::Storage, Some(&reason));
                        false
                    }
                }
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // Same failure mode as the tmux poller: without a respawn the in-flight
                // flag stays set and every structured row's status freezes.
                tracing::error!(
                    target: "tui.home",
                    "session feed worker gone; respawning a fresh feed",
                );
                self.session_feed = crate::tui::session_feed::SessionFeed::new();
                self.pending_session_feed = false;
                true
            }
        }
    }

    /// Fold one daemon-sourced structured status into the shared apply path, so sounds and
    /// status hooks fire as they do for a tmux-derived transition. Persistence is the
    /// deliberate exception: nothing about a structured row is the TUI's to write, so
    /// `persist_passive_status_transition` returns early for `is_structured()`. The status
    /// is a daemon-side overlay with no durable owner (#3201), and the automatic unread
    /// mark is the daemon's too, written from the live ACP turn-end event (#3181).
    ///
    /// The row is re-checked against `is_structured()` rather than trusted from the wire:
    /// the daemon's `view` and the local row's can disagree mid-conversion, and the tmux
    /// poller owns terminal rows, so dropping the mismatch keeps one producer per row.
    pub(in crate::tui) fn apply_daemon_status_update(
        &mut self,
        update: crate::tui::session_feed::DaemonStatusUpdate,
    ) {
        use crate::session::Status;
        use crate::tui::status_poller::IdleIntent;

        let (is_structured, applies, was_stopped, sunk) = match self.get_instance(&update.id) {
            Some(inst) => (
                inst.is_structured(),
                self.daemon_status_applies_to(inst),
                inst.status == Status::Stopped,
                inst.is_archived() || inst.is_trashed(),
            ),
            None => return,
        };
        if !is_structured || !applies {
            // Archived and trashed rows stay in `instances`, so their cached approvals
            // would outlive every state the daemon can refresh (the refresh path returns
            // here). Dropping the cache keeps the permission action from opening an
            // approval the resolver can only 404 on.
            if sunk {
                self.structured_pending_approvals.remove(&update.id);
            }
            return;
        }
        if update.pending_approvals.is_empty() {
            self.structured_pending_approvals.remove(&update.id);
        } else {
            self.structured_pending_approvals
                .insert(update.id.clone(), update.pending_approvals.clone());
        }
        // Lift a locally-`Stopped` row before the shared apply path sees it.
        // `apply_status_update`'s guard drops every update whose row is `Stopped`, which is
        // right for tmux rows but wrong here: stopping a structured session persists
        // `Stopped` and reopening it in the structured view does not clear that, so the
        // pill would stay grey through the whole next turn.
        //
        // The daemon has already applied its own stricter guard (only a `HealError` from
        // `AcpSessionAssigned` or `RateLimitAutoResumed` lifts `Stopped`, both emitted only
        // when a fresh worker attaches), so a non-`Stopped` reading provably means a new
        // worker epoch rather than a trailing post-stop event.
        if update.status != Status::Stopped && was_stopped {
            self.mutate_instance(&update.id, |inst| inst.status = Status::Idle);
        }
        self.apply_status_update(
            StatusUpdate {
                id: update.id,
                status: update.status,
                last_error: update.last_error,
                // Mirror the daemon's value rather than deriving one, so the TUI's idle
                // fade matches the web dashboard's instead of restarting on the first local
                // observation.
                idle_entered_at: match update.idle_entered_at {
                    Some(ts) => IdleIntent::Set(ts),
                    None => IdleIntent::Clear,
                },
                last_accessed_at: update.last_accessed_at,
                // Structured rows have no pane, so the Attention sort's
                // dead-pane tier never applies to them.
                pane_dead: false,
                live_status_baseline: Some(update.status),
                // A structured row has no pane to detect against.
                detection: None,
            },
            true,
            true,
        );
    }
    /// Queue a structured approval response without blocking input handling.
    pub(super) fn resolve_structured_approval(
        &mut self,
        session_id: String,
        nonce: String,
        choice: crate::tui::dialogs::PermissionResponseChoice,
    ) {
        let decision = Self::approval_decision_wire(choice);
        self.remove_structured_pending_approval(&session_id, &nonce);
        self.structured_approval_poller.request_resolve(
            crate::tui::approval_poller::ApprovalRequest {
                session_id,
                nonce,
                decision,
            },
        );
    }

    /// Map a dialog choice to the ACP wire decision. Pure and standalone so a
    pub(super) fn approval_decision_wire(
        choice: crate::tui::dialogs::PermissionResponseChoice,
    ) -> crate::acp::protocol::ApprovalDecisionWire {
        use crate::acp::protocol::ApprovalDecisionWire;
        use crate::tui::dialogs::PermissionResponseChoice;
        match choice {
            PermissionResponseChoice::Allow => ApprovalDecisionWire::Allow,
            PermissionResponseChoice::AllowAlways => ApprovalDecisionWire::AllowAlways,
            PermissionResponseChoice::Deny => ApprovalDecisionWire::Deny,
        }
    }

    /// Apply completed structured approval requests without blocking the TUI.
    pub fn apply_structured_approval_results(&mut self) -> bool {
        use crate::tui::approval_poller::StructuredApprovalPoller;
        use std::sync::mpsc::TryRecvError;

        let mut changed = false;
        loop {
            match self.structured_approval_poller.try_recv_result() {
                Ok(result) => {
                    changed = true;
                    self.apply_structured_approval_result(result);
                }
                Err(TryRecvError::Empty) => return changed,
                Err(TryRecvError::Disconnected) => {
                    tracing::error!(
                        target: "tui.home",
                        "structured approval worker gone; respawning a fresh worker",
                    );
                    self.structured_approval_poller = StructuredApprovalPoller::new();
                    return true;
                }
            }
        }
    }

    fn remove_structured_pending_approval(&mut self, session_id: &str, nonce: &str) {
        let remove_empty = self
            .structured_pending_approvals
            .get_mut(session_id)
            .is_some_and(|approvals| {
                approvals.retain(|pending| pending.nonce != nonce);
                approvals.is_empty()
            });
        if remove_empty {
            self.structured_pending_approvals.remove(session_id);
        }
    }

    pub(super) fn apply_structured_approval_result(
        &mut self,
        result: crate::tui::approval_poller::ApprovalResult,
    ) {
        use crate::tui::approval_poller::ApprovalResolution;

        match result.resolution {
            // Success: the card is answered, so clear it. The optimistic removal in
            // `resolve_structured_approval` already did, but a poll tick may have re-added
            // the nonce between submit and apply.
            ApprovalResolution::Resolved => {
                self.remove_structured_pending_approval(&result.session_id, &result.nonce);
            }
            // Already resolved elsewhere (the dashboard, or the server's compare-and-set
            // lost the race): clear it and say so, matching the structured view's feedback
            // instead of dropping it silently. Guarded so it can't stomp an info dialog the
            // user is mid-read on.
            ApprovalResolution::Gone => {
                self.remove_structured_pending_approval(&result.session_id, &result.nonce);
                if self.info_dialog.is_none() {
                    self.info_dialog = Some(InfoDialog::new(
                        "Already Resolved",
                        "This approval was already answered elsewhere.",
                    ));
                }
            }
            // Transient failure: leave the card cleared and surface the error. The still
            // pending approval comes back on the next 1 Hz daemon poll, so there is no
            // manual re-insert coupled to request order.
            ApprovalResolution::Failed(error) => {
                if self.info_dialog.is_none() {
                    self.info_dialog = Some(InfoDialog::new(
                        "Respond Failed",
                        &format!("Failed to resolve approval: {error}"),
                    ));
                }
            }
        }
    }

    /// Apply a single status update from the poller. Extracted from the loop in
    /// `apply_status_updates` so tests can drive the apply path without the background
    /// polling thread.
    pub(in crate::tui) fn apply_one_status_update(&mut self, update: StatusUpdate) {
        self.apply_status_update(update, true, true);
    }

    pub(in crate::tui) fn apply_status_updates_without_hooks(
        &mut self,
        updates: Vec<StatusUpdate>,
    ) {
        for update in updates {
            self.apply_status_update(update, false, false);
        }
    }

    pub(in crate::tui) fn reset_status_refresh(&mut self) {
        self.status_poller = StatusPoller::new();
        self.pending_status_refresh = false;
    }

    fn apply_status_update(&mut self, update: StatusUpdate, play_sound: bool, run_hooks: bool) {
        use crate::session::Status;

        let old_status = self.get_instance(&update.id).map(|i| i.status);
        let should_update = old_status.is_some_and(|s| {
            s != Status::Deleting
                && s != Status::Creating
                && s != Status::Stopped
                && update.status != Status::Stopped
        });

        let new_last_accessed = update.last_accessed_at;
        let new_pane_dead = update.pane_dead;

        if should_update {
            use crate::tui::status_poller::IdleIntent;

            let new_status = update.status;
            let new_error = update.last_error;
            let new_idle_entered_at = update.idle_entered_at;
            let new_live_status_baseline = update.live_status_baseline;
            let new_detection = update.detection;
            let status_changed = old_status != Some(new_status);
            self.mutate_instance(&update.id, |inst| {
                inst.status = new_status;
                // The daemon's `last_error` is authoritative only when present: an
                // incoming `Some` always applies, so an `Error -> Error` tick can replace
                // the old text (gating that on a status change froze the first error). A
                // `None` is not symmetric: the daemon tracks only ACP errors, so it cannot
                // tell "no error" from a locally-set message such as the delete-failure
                // text, and clearing every tick would wipe it. Clear only across a genuine
                // transition. See #3201.
                if let Some(err) = new_error {
                    inst.last_error = Some(err);
                } else if status_changed {
                    inst.last_error = None;
                }
                // Match on the producer's stated intent for `idle_entered_at` instead of
                // overloading `None`; see `IdleIntent` in `status_poller` for the
                // three-variant contract. See #2690.
                match new_idle_entered_at {
                    IdleIntent::Set(ts) => inst.idle_entered_at = Some(ts),
                    IdleIntent::Clear => inst.idle_entered_at = None,
                    IdleIntent::Keep => {}
                }
                if new_last_accessed.is_some() {
                    inst.last_accessed_at = new_last_accessed;
                }
                // A producer with no baseline yet (`None`) must not clear one the real
                // instance has, or every subsequent poll re-seeds from `None` and silently
                // disables restamping on real transitions. Locked by
                // [`apply_status_update_propagates_live_status_baseline_from_poller`].
                // See #2690.
                if let Some(baseline) = new_live_status_baseline {
                    inst.live_status_baseline = Some(baseline);
                }
                // The poller decided on a clone, so its detection bookkeeping reaches the
                // next poll only through here; `None` is a producer that never detected and
                // must not reset the row. See #3642.
                if let Some(detection) = new_detection {
                    inst.detection = detection;
                }
                inst.pane_dead_observed = new_pane_dead;
            });

            if let Some(old) = old_status {
                if old != new_status {
                    // Auto-mark unread when a turn finishes (Running -> Idle), unless the
                    // user is viewing this session in live-send. Runs in both apply paths,
                    // so a different session finishing while the user is attached elsewhere
                    // is still marked; the attached session is cleared on attach-return.
                    let is_live_target = self
                        .live_send
                        .as_ref()
                        .is_some_and(|s| s.session_id == update.id);
                    // Skip when already unread so a re-finishing session doesn't churn the
                    // flock once per turn.
                    let (already_unread, structured) = self
                        .get_instance(&update.id)
                        .map(|i| (i.is_unread(), i.is_structured()))
                        .unwrap_or((false, false));
                    // Structured rows are the daemon's: `should_mark_acp_unread` marks them
                    // off the live ACP turn-end event and persists it there (#3181), so
                    // marking here would be a second writer of the same boolean. The
                    // `is_live_target` exemption is always inert for them because
                    // `start_live_send` returns `None` for `is_structured()`, not because
                    // they lack a pane (they can own terminal and tool panes). What clears
                    // the mark for a structured row under the cursor is `tick_unread_dwell`,
                    // which re-checks `is_unread()` every tick.
                    let should_mark_unread = crate::session::unread_enabled()
                        && !structured
                        && old == Status::Running
                        && new_status == Status::Idle
                        && !is_live_target
                        && !already_unread;

                    // One flock for both the status/timestamp patch and the unread mark,
                    // matching the daemon's per-tick batching instead of two `Storage::update`
                    // calls on the same row.
                    self.persist_passive_status_transition(&update.id, should_mark_unread);
                    if should_mark_unread {
                        self.mutate_instance(&update.id, |inst| inst.mark_unread());
                    }

                    if let Some(inst) = self.get_instance(&update.id).cloned() {
                        self.handle_status_transition(
                            &inst, old, new_status, play_sound, run_hooks,
                        );
                    }
                }
            }
        } else if new_last_accessed.is_some() {
            self.mutate_instance(&update.id, |inst| {
                inst.last_accessed_at = new_last_accessed;
                inst.pane_dead_observed = new_pane_dead;
            });
        } else {
            // No status change and no fresh activity stamp, but pane_dead_observed still
            // needs refreshing: a corpse can sit unchanged for hours and the sort tier
            // should reflect reality. One bool write.
            self.mutate_instance(&update.id, |inst| {
                inst.pane_dead_observed = new_pane_dead;
            });
        }
    }

    pub(super) fn handle_status_transition(
        &self,
        inst: &Instance,
        old: crate::session::Status,
        new: crate::session::Status,
        play_sound: bool,
        run_hooks: bool,
    ) {
        if play_sound {
            crate::sound::play_for_transition(old, new, &self.sound_config);
        }
        if run_hooks {
            let hook_config = self.status_hook_config_for(inst);
            crate::status_hooks::run_for_transition(inst, old, new, &hook_config);
        }
    }

    pub(super) fn status_hook_config_for(
        &self,
        inst: &Instance,
    ) -> crate::status_hooks::StatusHookConfig {
        if self.active_profile.is_some() {
            return self.status_hook_config.clone();
        }
        let profile = inst.effective_profile();
        self.status_hook_configs
            .get(&profile)
            .cloned()
            .unwrap_or_else(|| self.status_hook_config.clone())
    }
}
