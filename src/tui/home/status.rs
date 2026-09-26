//! Canonical runtime projection and local system-health sampling.

use super::*;

impl HomeView {
    /// Rows eligible for local system-health sampling. Recovery owns its rows
    /// on a worker, so in-flight recoveries are skipped; a daemon start in
    /// flight only reserves the row and never skips the health sample.
    pub(in crate::tui) fn pollable_instances(&self) -> Vec<Instance> {
        self.instances
            .values()
            .filter(|i| !self.recovery_in_flight.contains(&i.id))
            .cloned()
            .collect()
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

    pub fn connect_runtime(&mut self) {
        self.set_sidebar_source(crate::tui::session_feed::SidebarSource::Connecting, None);
        self.session_feed
            .connect(self.active_profile.clone().unwrap_or_default());
    }

    /// Record where daemon-owned sidebar state comes from, logging the
    /// transition so a sidebar stuck on stale structured status is
    /// diagnosable from the log alone. `reason` says why the daemon is not
    /// the source and is ignored for `Daemon`.
    pub(super) fn set_sidebar_source(
        &mut self,
        source: crate::tui::session_feed::SidebarSource,
        reason: Option<&str>,
    ) -> bool {
        use crate::tui::session_feed::SidebarSource;

        if self.sidebar_source == source {
            return false;
        }
        self.sidebar_source = source;
        if source != SidebarSource::Daemon {
            self.cancel_native_attachment();
            self.teardown_live_send();
            self.structured_preview = None;
            self.preview_capture_worker = None;
            self.preview_capture_target = None;
            self.pending_paste = None;
        }
        match source {
            SidebarSource::Connecting => {
                tracing::info!(target: "tui.home", "sidebar: connecting to runtime")
            }
            SidebarSource::Daemon => tracing::info!(
                target: "tui.home",
                "sidebar: daemon reachable; rows follow /api/runtime/ws",
            ),
            SidebarSource::Disconnected => tracing::info!(
                target: "tui.home",
                reason = reason.unwrap_or(""),
                "sidebar: disconnected; session view unavailable",
            ),
        }
        true
    }

    /// Whether applying this runtime revision would outrun the local storage
    /// mirror. Status and pane observations are safe to apply in place, but
    /// durable identity/layout fields and removals must come from the locked
    /// storage load before the revision is marked applied.
    fn snapshot_requires_storage_reload(
        &mut self,
        snapshot: &crate::daemon::RuntimeSnapshot,
    ) -> bool {
        // The published ordering is the daemon's merged view (unknown
        // workspaces appended), so it cannot be compared to the persisted
        // file directly. A change to the persisted manual order is the thing
        // this view can still miss, so track what it last observed.
        let persisted = crate::session::load_workspace_ordering()
            .map(|ordering| ordering.order)
            .unwrap_or_default();
        if persisted != self.observed_workspace_ordering {
            self.observed_workspace_ordering = persisted;
            return true;
        }

        let row_ids: std::collections::HashSet<_> = snapshot
            .contents
            .sessions
            .iter()
            .map(|row| row.id.as_str())
            .collect();
        if self
            .in_flight_creation_id()
            .is_none_or(|id| !row_ids.contains(id))
            && self
                .instances
                .keys()
                .any(|id| !row_ids.contains(id.as_str()))
        {
            return true;
        }

        snapshot.contents.sessions.iter().any(|row| {
            // An unknown row in a tracked profile is handled by the caller's
            // addition check; one outside this view's scope is not its row.
            let Some(instance) = self.instances.get(&row.id) else {
                return false;
            };
            // Older/minimal runtime rows may omit descriptive metadata. Do
            // not manufacture changes from serde defaults on those rows.
            if row.title.is_empty() && row.profile.is_empty() {
                return false;
            }
            let expected_worktree = row
                .has_managed_worktree
                .then_some(row.branch.as_deref())
                .flatten();
            let actual_worktree = instance
                .worktree_info
                .as_ref()
                .map(|worktree| worktree.branch.as_str());
            instance.title != row.title
                || instance.group_path != row.group_path
                || (!row.profile.is_empty() && instance.source_profile != row.profile)
                || instance.tool != row.tool
                || instance.view != row.view
                || instance.base_branch_override != row.base_branch_override
                || actual_worktree != expected_worktree
        })
    }

    /// Surface every command error the feed drained, and return whether there
    /// was anything to surface.
    ///
    /// The single sink for both drain sites (`apply_session_feed` and
    /// `apply_restart_results`): draining the feed is destructive, so an error
    /// presented anywhere else would be a diagnostic no one ever saw. Nothing
    /// is dropped here:
    ///
    /// - every error's message is rendered, including the ones whose id drives
    ///   the indeterminate dialog, so a multi-error batch is fully readable;
    /// - EVERY unknown-outcome id is queued, not just the first. The row stays
    ///   quarantined until the user resolves it, so keeping only the head would
    ///   leave the rest blocked with no dialog left to release them.
    pub(super) fn present_command_errors(
        &mut self,
        errors: Vec<crate::tui::session_feed::SessionCommandError>,
    ) -> bool {
        if errors.is_empty() {
            return false;
        }
        for error in &errors {
            if self
                .pending_archive_cursor
                .as_ref()
                .is_some_and(|pending| pending.id == error.id)
            {
                self.pending_archive_cursor = None;
            }
            if error.marks_unread && self.manual_unread_hold.as_deref() == Some(&error.id) {
                self.manual_unread_hold = None;
            }
            if !error.outcome_unknown {
                continue;
            }
            let known = self
                .pending_indeterminate_resolution
                .as_deref()
                .is_some_and(|id| id == error.id)
                || self
                    .pending_indeterminate_queue
                    .iter()
                    .any(|(id, _)| *id == error.id);
            if !known {
                self.pending_indeterminate_queue
                    .push((error.id.clone(), error.message.clone()));
            }
        }

        // The full batch, so a diagnostic shown next to the indeterminate
        // dialog is not the only trace of the other failures in this drain.
        let diagnostics = errors
            .iter()
            .map(|error| format!("{}: {}", error.id, error.message))
            .collect::<Vec<_>>()
            .join("\n");

        if let Some((id, message)) = self.pending_indeterminate_queue.first().cloned() {
            self.open_indeterminate_dialog(&id, &message);
            if errors.len() > 1 {
                self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                    "Runtime change",
                    &diagnostics,
                ));
            }
        } else {
            self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                "Runtime change",
                &diagnostics,
            ));
        }
        true
    }

    /// Promote the next queued unknown-outcome id to a dialog, if any is
    /// waiting. Called after one is resolved so a batch of them all get their
    /// unlock prompt.
    pub(super) fn promote_next_indeterminate(&mut self) {
        let Some((id, message)) = self.pending_indeterminate_queue.first().cloned() else {
            return;
        };
        self.open_indeterminate_dialog(&id, &message);
    }

    fn open_indeterminate_dialog(&mut self, id: &str, message: &str) {
        self.pending_indeterminate_resolution = Some(id.to_string());
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                "Resolve Unknown Outcome",
                &format!(
                    "The previous runtime change for '{id}' has an unknown outcome: {message}\n\nVerify the current canonical state, then unlock this row for a new action. No mutation is submitted by this resolution."
                ),
                "resolve_indeterminate",
            )
            .buttons("Unlock", "Keep Blocked"),
        );
    }

    /// Apply a pending session-list result from the daemon. Returns true if
    /// the caller should redraw.
    pub fn apply_session_feed(&mut self) -> bool {
        use crate::tui::session_feed::{SessionFeedResult, SidebarSource};
        use std::sync::mpsc::TryRecvError;

        let mut snapshot_applied = false;
        let updated = match self.session_feed.try_recv() {
            Ok(result) => match result {
                SessionFeedResult::Snapshot(snapshot) => {
                    let mut metadata_changed = false;
                    // Rows load from storage rather than from the wire
                    // projection, so a revision that adds, renames, moves,
                    // re-renders, or drops a row is reconciled from the locked
                    // storage load before this revision is marked applied. A
                    // reload also keeps a still-unpublished creating stub alive.
                    metadata_changed |=
                        self.reconcile_in_flight_creation(&snapshot.contents.sessions);
                    let in_flight = self.in_flight_creation_id();
                    let unknown_row = snapshot.contents.sessions.iter().any(|row| {
                        Some(row.id.as_str()) != in_flight
                            && !self.instances.contains_key(&row.id)
                            && self.storages.contains_key(&row.profile)
                    });
                    if unknown_row || self.snapshot_requires_storage_reload(&snapshot) {
                        match self.reload() {
                            Ok(()) => metadata_changed = true,
                            Err(error) => tracing::warn!(
                                target: "tui.session_feed",
                                %error,
                                "reload before applying a canonical runtime revision failed"
                            ),
                        }
                    }
                    for row in &snapshot.contents.sessions {
                        metadata_changed |= self.apply_daemon_status_update(row);
                        let Some(instance) = self.instances.get_mut(&row.id) else {
                            continue;
                        };
                        if instance.agent_pane != row.agent_pane {
                            instance.agent_pane.clone_from(&row.agent_pane);
                            metadata_changed = true;
                        }
                        if instance.auxiliary != row.auxiliary {
                            instance.auxiliary.clone_from(&row.auxiliary);
                            metadata_changed = true;
                        }
                        if instance.unread != row.unread {
                            instance.unread = row.unread;
                            metadata_changed = true;
                            if !row.unread && self.manual_unread_hold.as_deref() == Some(&row.id) {
                                self.manual_unread_hold = None;
                            }
                        }
                        for (raw, current) in [
                            (row.archived_at.as_deref(), &mut instance.archived_at),
                            (row.favorited_at.as_deref(), &mut instance.favorited_at),
                            (row.snoozed_until.as_deref(), &mut instance.snoozed_until),
                            (row.pinned_at.as_deref(), &mut instance.pinned_at),
                            (
                                row.last_accessed_at.as_deref(),
                                &mut instance.last_accessed_at,
                            ),
                            (
                                row.idle_entered_at.as_deref(),
                                &mut instance.idle_entered_at,
                            ),
                            (
                                row.idle_dormant_since.as_deref(),
                                &mut instance.idle_dormant_since,
                            ),
                        ] {
                            let next = match raw {
                                Some(value) => match chrono::DateTime::parse_from_rfc3339(value) {
                                    Ok(value) => Some(value.with_timezone(&chrono::Utc)),
                                    Err(_) => continue,
                                },
                                None => None,
                            };
                            if *current != next {
                                *current = next;
                                metadata_changed = true;
                            }
                        }
                    }
                    if metadata_changed {
                        self.rebuild_flat_items();
                        self.reseat_cursor_after_rebuild();
                        // A rebuild reorders rows under the cursor: resolve the
                        // selection for the row that now sits there, so a key
                        // pressed right after a canonical frame acts on what the
                        // user sees instead of on nothing.
                        self.update_selected();
                    }
                    metadata_changed |= self.set_sidebar_source(SidebarSource::Daemon, None);
                    if !self.structured_pending_approvals.is_empty() {
                        let ids: std::collections::HashSet<_> = snapshot
                            .contents
                            .sessions
                            .iter()
                            .map(|row| row.id.as_str())
                            .collect();
                        let count = self.structured_pending_approvals.len();
                        self.structured_pending_approvals
                            .retain(|id, _| ids.contains(id.as_str()));
                        metadata_changed |= count != self.structured_pending_approvals.len();
                    }
                    metadata_changed |= self.apply_pending_archive_cursor();
                    metadata_changed |= self.session_feed.mark_snapshot_applied(snapshot);
                    if !self.session_feed.native_interaction_available() {
                        self.cancel_native_attachment();
                        self.teardown_live_send();
                        self.pending_paste = None;
                    }
                    snapshot_applied = true;
                    metadata_changed
                }
                SessionFeedResult::Unavailable(reason) => {
                    self.set_sidebar_source(SidebarSource::Disconnected, Some(&reason))
                }
            },
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => false,
        };
        let drained = self.session_feed.drain_command_errors();
        let command_error = self.present_command_errors(drained);
        if snapshot_applied {
            if let Some(id) = self
                .live_send
                .as_ref()
                .filter(|live| {
                    self.get_instance(&live.session_id)
                        .is_some_and(|instance| instance.is_unread())
                        && self.session_feed.can_submit(&live.session_id)
                })
                .map(|live| live.session_id.clone())
            {
                self.clear_unread_on_view(&id);
            }
        }
        updated || command_error
    }

    pub(super) fn auxiliary_presence_for_view(
        &self,
        instance: &Instance,
    ) -> crate::session::PanePresence {
        use crate::session::{AuxiliaryTarget, PanePresence};
        match &self.view_mode {
            ViewMode::Terminal => {
                let target = if instance.is_sandboxed()
                    && self.get_terminal_mode(&instance.id) == TerminalMode::Container
                {
                    AuxiliaryTarget::Container { index: 0 }
                } else {
                    AuxiliaryTarget::Host { index: 0 }
                };
                instance.auxiliary_presence(&target)
            }
            ViewMode::Tool(name) => instance.tool_presence(name),
            ViewMode::Structured => PanePresence::Unknown,
        }
    }

    pub(in crate::tui) fn apply_daemon_status_update(
        &mut self,
        row: &crate::daemon::SessionResponse,
    ) -> bool {
        let Some(status) = crate::session::Status::from_api_str(&row.status) else {
            return false;
        };
        let Some(instance) = self.instances.get_mut(&row.id) else {
            return false;
        };
        let mut changed = instance.status != status
            || instance.last_error != row.last_error
            || instance.pane_dead_observed != row.pane_dead_observed;
        if instance.status != status {
            crate::sound::play_for_transition(instance.status, status, &self.sound_config);
        }
        instance.status = status;
        instance.last_error.clone_from(&row.last_error);
        instance.pane_dead_observed = row.pane_dead_observed;
        if row.view != crate::session::View::Structured
            || row.archived_at.is_some()
            || row.trashed_at.is_some()
            || row.pending_approvals.is_empty()
        {
            changed |= self.structured_pending_approvals.remove(&row.id).is_some();
        } else if self.structured_pending_approvals.get(&row.id) != Some(&row.pending_approvals) {
            self.structured_pending_approvals
                .insert(row.id.clone(), row.pending_approvals.clone());
            changed = true;
        }
        changed
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
            // Success: the card is answered, clear it. The optimistic removal
            // in `resolve_structured_approval` already did this; re-run it in
            // case a poll tick re-added the nonce between submit and apply.
            ApprovalResolution::Resolved => {
                self.remove_structured_pending_approval(&result.session_id, &result.nonce);
            }
            // Already resolved elsewhere (dashboard, or the server's
            // compare-and-set lost the race). Clear it and say so, matching
            // the structured view's "approval already resolved" feedback
            // instead of silently dropping it. Guarded so it can't stomp an
            // info dialog the user is mid-read on.
            ApprovalResolution::Gone => {
                self.remove_structured_pending_approval(&result.session_id, &result.nonce);
                if self.info_dialog.is_none() {
                    self.info_dialog = Some(InfoDialog::new(
                        "Already Resolved",
                        "This approval was already answered elsewhere.",
                    ));
                }
            }
            // Transient failure: leave the card cleared and surface the error.
            // The still-pending approval will be restored by the next 1 Hz
            // daemon poll (the server still lists it), so there is no manual
            // re-insert to couple to request order.
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
}
