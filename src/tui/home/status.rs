//! Status and metrics refresh: what the pollers report and how a row's
//! status is applied and hooked.

use super::*;

impl HomeView {
    /// Snapshot of `self.instances` eligible for status polling.
    /// In-flight recovery and restart candidates are excluded; their
    /// post-cascade `Instance` arrives via `apply_recovery_updates` /
    /// `apply_restart_results` and skipping the parallel poll prevents racing
    /// transitions during the suppression window.
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
                // The worker thread is gone (a panic in poll_statuses_once).
                // Without a respawn, pending_status_refresh stays set and
                // request_status_refresh never fires again, freezing every
                // session's live status for the rest of the process.
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
                // The sampler thread died (a panic in sample_memory /
                // count_running_agents). Respawn so pending_metrics_refresh
                // does not stay stuck and freeze the strip.
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

    /// Request the daemon's view of every structured row's status
    /// (non-blocking). Skipped entirely when no structured session is
    /// loaded, so a terminal-only home view never talks to the daemon.
    pub fn request_daemon_status_refresh(&mut self) {
        if self.pending_daemon_status_refresh {
            return;
        }
        if !self.instances.values().any(|i| i.is_structured()) {
            return;
        }
        self.daemon_status_poller.request_refresh();
        self.pending_daemon_status_refresh = true;
    }

    /// Whether a daemon-sourced status may be applied to `id`, mirroring the
    /// exclusions the tmux producer applies. A row mid-restart or
    /// mid-recovery-cascade has its post-cascade `Instance` delivered by
    /// `apply_restart_results` / `apply_recovery_updates`; letting the daemon's
    /// copy land during that window races those transitions, so both are
    /// excluded here just as [`Self::pollable_instances`] excludes them from the
    /// tmux poller. Recovery already skips structured rows
    /// (`recovery::is_recovery_candidate`), so in practice this is the restart
    /// guard, but both are checked so the two producers stay symmetrical.
    ///
    /// Archived and trashed rows are also excluded. `/api/sessions` returns
    /// them unfiltered, and the `is_archived()` short-circuit that keeps the
    /// tmux producer off a sunk row lives in
    /// `session::instance::status_update::update_status_with_metadata_inner`,
    /// which this daemon path never reaches; without this a sunk row would be
    /// restamped and re-marked unread. See #3201 / #1868 / #2206.
    ///
    /// The cost of that exclusion: a sunk structured row that is already in
    /// `Status::Error` now has no producer able to clear it. The daemon is
    /// excluded here, the tmux poller bails on structured rows before probing
    /// (`status_poller.rs`), and `reload_storage_only` carries `prev.status`
    /// forward across reloads, so the stale value survives. It stays visible
    /// because `agent_row_icon` lets `Error` and `Deleting` punch through the
    /// sunk-row mask on purpose (a failed permanent delete has to remain
    /// legible). The tmux producer has the same property via its own
    /// `is_archived()` short-circuit, so this is consistent rather than new,
    /// but unarchiving is the only way back.
    fn daemon_status_applies_to(&self, inst: &Instance) -> bool {
        !self.recovery_in_flight.contains(&inst.id)
            && !self.restart_in_flight.contains(&inst.id)
            && !inst.is_archived()
            && !inst.is_trashed()
    }

    /// Apply any pending daemon-sourced statuses. Returns true if the
    /// caller should redraw.
    pub fn apply_daemon_status_updates(&mut self) -> bool {
        use std::sync::mpsc::TryRecvError;

        match self.daemon_status_poller.try_recv_updates() {
            Ok(updates) => {
                let applied = !updates.is_empty();
                for update in updates {
                    self.apply_daemon_status_update(update);
                }
                self.pending_daemon_status_refresh = false;
                applied
            }
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => {
                // Same failure mode as the tmux poller: without a respawn the
                // in-flight flag stays set and every structured row's status
                // freezes for the rest of the process.
                tracing::error!(
                    target: "tui.home",
                    "daemon status poller worker gone; respawning a fresh poller",
                );
                self.daemon_status_poller =
                    crate::tui::daemon_status_poller::DaemonStatusPoller::new();
                self.pending_daemon_status_refresh = false;
                true
            }
        }
    }

    /// Fold one daemon-sourced structured status into the shared apply path,
    /// so sounds and status hooks fire exactly as they do for a tmux-derived
    /// transition. Persistence is the deliberate exception: this path only
    /// handles structured rows, and nothing about one is the TUI's to write, so
    /// `persist_passive_status_transition` returns early for `is_structured()`.
    /// The status is a daemon-side overlay with no durable owner (#3201), and
    /// the automatic unread mark is the daemon's too, written from the live ACP
    /// turn-end event (#3181).
    ///
    /// The row is re-checked against `is_structured()` here rather than
    /// trusted from the wire: the daemon's `view` and the local row's could
    /// disagree for a session mid-conversion, and the tmux poller owns
    /// terminal rows. Dropping the mismatch keeps one producer per row.
    pub(in crate::tui) fn apply_daemon_status_update(
        &mut self,
        update: crate::tui::daemon_status_poller::DaemonStatusUpdate,
    ) {
        use crate::session::Status;
        use crate::tui::status_poller::IdleIntent;

        // One lookup feeds both guards below and the Stopped-lift check.
        let Some(inst) = self.get_instance(&update.id) else {
            return;
        };
        if !inst.is_structured() || !self.daemon_status_applies_to(inst) {
            return;
        }
        let was_stopped = inst.status == Status::Stopped;
        // Lift a locally-`Stopped` row before the shared apply path sees it.
        // `apply_status_update`'s guard drops every update whose row is
        // `Stopped`, which is right for tmux rows (nothing but an explicit
        // start should wake one) but wrong here: stopping a structured session
        // persists `Stopped`, and reopening it in the structured view does not
        // clear that (`open_structured_view` only mounts the view), so without
        // this the pill stays grey through the whole next turn, which is the
        // bug this producer exists to fix.
        //
        // The daemon has already applied its own, stricter `Stopped` guard
        // (`apply_status_intent`: only a `HealError` from `AcpSessionAssigned`
        // or `RateLimitAutoResumed` lifts `Stopped`, and both are emitted only
        // when a fresh worker attaches). So a non-`Stopped` reading from the
        // daemon provably means a new worker epoch, never a trailing
        // post-stop event. Reproducing the daemon's own Stopped -> Idle step
        // here keeps the two ladders identical.
        if update.status != Status::Stopped && was_stopped {
            self.mutate_instance(&update.id, |inst| inst.status = Status::Idle);
        }
        self.apply_status_update(
            StatusUpdate {
                id: update.id,
                status: update.status,
                last_error: update.last_error,
                // Mirror the daemon's own value rather than deriving one, so
                // the TUI's idle fade matches the web dashboard's for the
                // same session instead of restarting on the first local
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

    /// Apply a single status update from the poller. Extracted from the
    /// channel-pulling loop in `apply_status_updates` so tests can drive
    /// the apply path directly without having to push through the
    /// background polling thread.
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
                // The daemon's `last_error` is authoritative only when present:
                // an incoming `Some` is always applied, so an `Error -> Error`
                // tick can replace the old text. Gating that write on a status
                // change froze the first error on the row. A `None` is not
                // symmetric: the daemon tracks only ACP errors, so it cannot
                // distinguish "no error" from a locally-set message such as the
                // delete-failure text from `apply_deletion_results`, and
                // clearing on every unchanged tick would wipe it. Clear only
                // across a genuine transition, leaving a stale same-status
                // message in place until then. See #3201.
                if let Some(err) = new_error {
                    inst.last_error = Some(err);
                } else if status_changed {
                    inst.last_error = None;
                }
                // Match on the producer's stated intent for `idle_entered_at`
                // instead of overloading `None`. See `IdleIntent` in
                // `status_poller` for the three-variant contract that
                // replaces the pre-fix `Option<DateTime<Utc>>` (which
                // conflated "producer observed a transition out of Idle" with
                // "producer has no observation"). See #2690.
                match new_idle_entered_at {
                    IdleIntent::Set(ts) => inst.idle_entered_at = Some(ts),
                    IdleIntent::Clear => inst.idle_entered_at = None,
                    IdleIntent::Keep => {}
                }
                if new_last_accessed.is_some() {
                    inst.last_accessed_at = new_last_accessed;
                }
                // A producer that has no baseline yet (`None`) must not
                // clear one the real instance already has, or every
                // subsequent poll of that instance re-seeds from `None`
                // and silently disables restamping on real transitions.
                // Locked by
                // [`apply_status_update_propagates_live_status_baseline_from_poller`]
                // in `src/tui/home/tests.rs`. See #2690.
                if let Some(baseline) = new_live_status_baseline {
                    inst.live_status_baseline = Some(baseline);
                }
                // The poller decided on a clone, so its detection bookkeeping
                // only reaches the next poll through here. `None` is a
                // producer that never detected; it must not reset the row.
                // See #3642.
                if let Some(detection) = new_detection {
                    inst.detection = detection;
                }
                inst.pane_dead_observed = new_pane_dead;
            });

            if let Some(old) = old_status {
                if old != new_status {
                    // Auto-mark unread when a turn finishes (Running ->
                    // Idle), unless the user is currently viewing this
                    // session in live-send. This runs in both the with-
                    // and without-hooks apply paths, so a *different*
                    // session finishing while the user is attached
                    // elsewhere still gets marked. The attached session
                    // itself is cleared on attach-return, so a turn that
                    // finishes during an attach nets to read.
                    let is_live_target = self
                        .live_send
                        .as_ref()
                        .is_some_and(|s| s.session_id == update.id);
                    // Skip when already unread (the mark is a no-op) so a
                    // re-finishing session doesn't churn the flock once
                    // per turn.
                    let (already_unread, structured) = self
                        .get_instance(&update.id)
                        .map(|i| (i.is_unread(), i.is_structured()))
                        .unwrap_or((false, false));
                    // Structured rows are the daemon's: `should_mark_acp_unread`
                    // marks them off the live ACP turn-end event and persists it
                    // there (#3181). Marking here too would be a second writer of
                    // the same boolean for no gain, and `is_live_target` cannot
                    // even earn its keep on one: `start_live_send` returns `None`
                    // outright for `is_structured()` (`home/input.rs`, matched by
                    // the guard in `app.rs`), so the exemption is always inert for
                    // them. Note it is that explicit guard which makes it inert,
                    // not the absence of a pane: a structured row can own paired
                    // terminal and tool panes, so `LiveSendTarget` alone would not
                    // rule live-send out. What clears the mark for a structured
                    // row the user is actually reading is `tick_unread_dwell`,
                    // which re-checks `is_unread()` every tick and so picks up a
                    // daemon-written mark on the row under the cursor.
                    let should_mark_unread = crate::session::unread_enabled()
                        && !structured
                        && old == Status::Running
                        && new_status == Status::Idle
                        && !is_live_target
                        && !already_unread;

                    // One flock for both the status/timestamp patch and the
                    // unread mark, matching the daemon's per-tick batching
                    // shape (server/status_poll.rs's status_poll_loop) instead of
                    // two separate Storage::update calls on the same row.
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
            // No status change AND no fresh activity stamp. We still
            // need to refresh pane_dead_observed: a corpse can sit
            // unchanged for hours and the sort tier should reflect
            // current reality. Cheap mutate (one bool write).
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
