//! Canonical runtime projection and local system-health sampling.

use super::*;
use crate::session::Status;

impl HomeView {
    /// Local health samples observe every canonical row; daemon recovery owns mutations.
    pub(in crate::tui) fn pollable_instances(&self) -> Vec<Instance> {
        self.instances.values().cloned().collect()
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

    pub fn connect_runtime(&mut self) {
        self.session_feed_reload_retry_at = None;
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

        let reason = (source == SidebarSource::Disconnected).then(|| {
            reason
                .filter(|reason| !reason.is_empty())
                .unwrap_or("The daemon could not be reached.")
        });
        let cached_reason = self
            .runtime_failure_message
            .as_deref()
            .and_then(|message| message.strip_prefix("Runtime unavailable\n\n"))
            .and_then(|message| {
                message.strip_suffix("\n\nr: Reconnect / start local runtime\nq: Quit")
            });
        if self.sidebar_source == source && reason == cached_reason {
            return false;
        }
        self.runtime_failure_message = reason.map(|reason| {
            format!(
                "Runtime unavailable\n\n{reason}\n\nr: Reconnect / start local runtime\nq: Quit"
            )
        });
        self.sidebar_source = source;
        if source == SidebarSource::Daemon {
            self.project_registry_authoritative = true;
        }
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
        &self,
        snapshot: &crate::daemon::RuntimeSnapshot,
        persisted_ordering: &[String],
    ) -> bool {
        // The daemon appends unknown workspaces; acknowledge the persisted manual order instead.
        if persisted_ordering != self.observed_workspace_ordering.as_slice() {
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

        let namespace_changed = self.session_feed.applied_snapshot().is_none_or(|applied| {
            applied.contents.profiles.len() != snapshot.contents.profiles.len()
                || snapshot.contents.profiles.iter().any(|incoming| {
                    applied
                        .contents
                        .profiles
                        .iter()
                        .find(|previous| previous.name == incoming.name)
                        .is_none_or(|previous| previous.groups != incoming.groups)
                })
                || applied.contents.sessions.len() != snapshot.contents.sessions.len()
                || applied
                    .contents
                    .sessions
                    .iter()
                    .zip(&snapshot.contents.sessions)
                    .any(|(previous, row)| {
                        previous.id != row.id
                            || previous.workspace_repos != row.workspace_repos
                            || previous.workspace_dir != row.workspace_dir
                            || previous.workspace_branch != row.workspace_branch
                            || previous.workspace_created_at != row.workspace_created_at
                            || previous.workspace_cleanup_on_delete
                                != row.workspace_cleanup_on_delete
                            || previous.is_sandboxed != row.is_sandboxed
                            || previous.sandbox_container_name != row.sandbox_container_name
                    })
        });
        if namespace_changed {
            return true;
        }
        snapshot.contents.sessions.iter().any(|row| {
            // An unknown row in a tracked profile is handled by the caller's
            // addition check; one outside this view's scope is not its row.
            let Some(instance) = self.instances.get(&row.id) else {
                return false;
            };

            let expected_worktree = row
                .has_managed_worktree
                .then_some(row.branch.as_deref())
                .flatten();
            let actual_worktree = instance
                .worktree_info
                .as_ref()
                .map(|worktree| worktree.branch.as_str());
            instance.color != row.color
                || instance.command != row.command
                || instance.extra_args != row.extra_args
                || instance.yolo_mode != row.yolo_mode
                || instance.scratch != row.scratch
                || instance.notify_on_waiting != row.notify_on_waiting
                || instance.notify_on_idle != row.notify_on_idle
                || instance.notify_on_error != row.notify_on_error
                || instance.project_path != row.project_path
                || instance.sort_index != row.sort_index
                || instance.agent_session_id != row.agent_session_id
                || instance.lifecycle_generation != row.lifecycle_generation
                || instance.lifecycle_reservation != row.lifecycle_reservation
                || instance.title != row.title
                || instance.group_path != row.group_path
                || instance.source_profile != row.profile
                || instance.tool != row.tool
                || instance.view != row.view
                || instance.base_branch_override != row.base_branch_override
                || actual_worktree != expected_worktree
        })
    }

    /// Present every drained error and retain every unknown-outcome ID until resolution.
    /// Both feed-drain paths use this sink so diagnostics and quarantines cannot be lost.
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

    fn open_indeterminate_dialog(&mut self, id: &str, message: &str) {
        self.pending_indeterminate_resolution = Some(id.to_string());
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                "Resolve Unknown Outcome",
                &format!(
                    "The previous runtime change for '{id}' has an unknown outcome: {message}\n\nVerify the current canonical state, then unlock this row for a new action. Resolution submits no mutation. To reopen after Keep Blocked, use Ctrl+K: Resolve unknown runtime change."
                ),
                "resolve_indeterminate",
            )
            .buttons("Unlock", "Keep Blocked"),
        );
    }

    pub(super) fn apply_canonical_projection(
        &mut self,
        snapshot: &crate::daemon::RuntimeSnapshot,
        refresh_metadata: bool,
    ) -> anyhow::Result<()> {
        let mut next = indexmap::IndexMap::new();
        // Discover this creation's reservation without mutating the displayed model.
        let in_flight = self.in_flight_creation_id().or_else(|| {
            let pending = self.pending_creation.as_ref()?;
            if pending.confirmation.is_some() {
                return None;
            }
            snapshot
                .contents
                .sessions
                .iter()
                .find(|row| row.idempotency_key.as_deref() == Some(pending.request_key.as_str()))
                .map(|row| row.id.as_str())
        });
        let mut opened_storages = HashMap::new();
        let mut profile_loads = HashMap::new();
        for profile in &snapshot.contents.profiles {
            if self
                .active_profile
                .as_ref()
                .is_some_and(|active| *active != profile.name)
            {
                continue;
            }
            if !self.storages.contains_key(&profile.name) {
                opened_storages.insert(
                    profile.name.clone(),
                    Storage::open(&profile.name, self.file_watch.clone())?,
                );
            }
            let needs_metadata = refresh_metadata
                || snapshot.contents.sessions.iter().any(|row| {
                    row.profile == profile.name
                        && in_flight != Some(row.id.as_str())
                        && self
                            .instances
                            .get(&row.id)
                            .is_none_or(|local| local.source_profile != row.profile)
                });
            if needs_metadata {
                let storage = self
                    .storages
                    .get(&profile.name)
                    .or_else(|| opened_storages.get(&profile.name))
                    .expect("profile store staged");
                profile_loads.insert(profile.name.clone(), storage.load_complete_with_groups()?.0);
            }
        }
        let duplicate_reports = if refresh_metadata {
            let loads: Vec<(&str, &[Instance])> = profile_loads
                .iter()
                .map(|(name, rows)| (name.as_str(), rows.as_slice()))
                .collect();
            let stores: Vec<(&str, &Storage)> = self
                .storages
                .iter()
                .chain(opened_storages.iter())
                .map(|(name, storage)| (name.as_str(), storage))
                .collect();
            Some(crate::session::duplicate_reports(&loads, &stores))
        } else {
            None
        };
        // Each profile is loaded once; consume its owned rows instead of copying them again.
        let mut persisted_profiles: HashMap<_, _> = profile_loads
            .into_iter()
            .map(|(name, rows)| (name, Self::build_instances_map(rows)))
            .collect();
        for row in &snapshot.contents.sessions {
            if self
                .active_profile
                .as_ref()
                .is_some_and(|active| *active != row.profile)
                || in_flight == Some(row.id.as_str())
            {
                continue;
            }
            let reports = duplicate_reports
                .as_deref()
                .unwrap_or(&self.legacy_duplicate_reports);
            anyhow::ensure!(
                !reports.iter().any(|report| report.id == row.id),
                "Canonical session '{}' has ambiguous native metadata copies",
                row.id
            );
            // A refreshed profile must contain the row, even if the old display did.
            // Never manufacture native worktree/conversation metadata from a wire row.
            let mut instance = if let Some(rows) = persisted_profiles.get_mut(&row.profile) {
                let mut instance = rows.swap_remove(&row.id).ok_or_else(|| {
                    anyhow::anyhow!("Canonical session '{}' is not available in the read-only metadata projection", row.id)
                })?;
                if let Some(previous) = self.instances.get(&row.id) {
                    instance.merge_runtime_from_reload(previous);
                }
                instance
            } else {
                self.instances.get(&row.id).filter(|local| local.source_profile == row.profile)
                    .cloned().ok_or_else(|| {
                        anyhow::anyhow!("Canonical session '{}' is not available in the read-only metadata projection", row.id)
                    })?
            };
            instance.title.clone_from(&row.title);
            instance.project_path.clone_from(&row.project_path);
            instance.source_profile.clone_from(&row.profile);
            instance.group_path.clone_from(&row.group_path);
            instance.sort_index = row.sort_index;
            instance.tool.clone_from(&row.tool);
            instance.command.clone_from(&row.command);
            instance.extra_args.clone_from(&row.extra_args);
            instance.view = row.view;
            instance
                .base_branch_override
                .clone_from(&row.base_branch_override);
            instance.agent_session_id.clone_from(&row.agent_session_id);
            instance.lifecycle_generation = row.lifecycle_generation;
            instance
                .lifecycle_reservation
                .clone_from(&row.lifecycle_reservation);
            instance.agent_pane.clone_from(&row.agent_pane);
            instance.auxiliary.clone_from(&row.auxiliary);
            instance.unread = row.unread;
            instance.color.clone_from(&row.color);
            instance.yolo_mode = row.yolo_mode;
            instance.scratch = row.scratch;
            instance.notify_on_waiting = row.notify_on_waiting;
            instance.notify_on_idle = row.notify_on_idle;
            instance.notify_on_error = row.notify_on_error;
            instance.last_error.clone_from(&row.last_error);
            instance.pane_dead_observed = row.pane_dead_observed;
            if let Some(status) = Status::from_api_str(&row.status) {
                instance.status = status;
            }
            if let Some(worktree) = instance.worktree_info.as_mut() {
                if let Some(branch) = &row.branch {
                    worktree.branch.clone_from(branch);
                }
            }
            for (raw, current) in [
                (row.trashed_at.as_deref(), &mut instance.trashed_at),
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
                *current = raw
                    .map(chrono::DateTime::parse_from_rfc3339)
                    .transpose()?
                    .map(|stamp| stamp.with_timezone(&chrono::Utc));
            }
            next.insert(row.id.clone(), instance);
        }
        if let Some(stub) = self
            .creating_stub_id
            .as_ref()
            .and_then(|id| self.instances.get(id))
            .cloned()
        {
            next.insert(stub.id.clone(), stub);
        }
        let mut trees = HashMap::new();
        for profile in &snapshot.contents.profiles {
            if self
                .active_profile
                .as_ref()
                .is_some_and(|active| *active != profile.name)
            {
                continue;
            }
            let rows: Vec<_> = next
                .values()
                .filter(|row| row.source_profile == profile.name)
                .cloned()
                .collect();
            trees.insert(
                profile.name.clone(),
                GroupTree::new_with_groups(&rows, &profile.groups),
            );
        }
        // No displayed row, approval, selection, or sound changes before every read and parse succeeds.
        for (id, instance) in &next {
            if let Some(previous) = self.instances.get(id) {
                if previous.status != instance.status {
                    crate::sound::play_for_transition(
                        previous.status,
                        instance.status,
                        &self.sound_config,
                    );
                }
            }
        }
        self.instances = next;
        self.group_trees = trees;
        self.storages.extend(opened_storages);
        self.storages.retain(|name, _| {
            snapshot.contents.profiles.iter().any(|profile| {
                profile.name == *name
                    && self
                        .active_profile
                        .as_ref()
                        .is_none_or(|active| active == name)
            })
        });
        if let Some(reports) = duplicate_reports {
            log_legacy_duplicates_once(&reports);
            self.legacy_duplicate_reports = reports;
            self.remote_owner_cache.borrow_mut().clear();
            let mut disk_profiles: Vec<_> = self.storages.keys().cloned().collect();
            disk_profiles.sort();
            self.rewire_disk_subscriptions(&disk_profiles);
            let config_profiles: Vec<_> = snapshot
                .contents
                .profiles
                .iter()
                .map(|profile| profile.name.clone())
                .collect();
            self.rewire_config_subscriptions(&config_profiles);
        }
        self.reconcile_in_flight_creation(&snapshot.contents.sessions);
        self.refresh_registered_projects();
        Ok(())
    }

    /// Apply a pending session-list result from the daemon. Returns true if
    /// the caller should redraw.
    pub fn apply_session_feed(&mut self) -> bool {
        use crate::tui::session_feed::{SessionFeedResult, SidebarSource};
        use std::sync::mpsc::TryRecvError;

        let pending_before =
            self.session_feed.any_pending() || self.pending_namespace_intent.is_some();
        let mut snapshot_applied = false;
        let updated = match self.session_feed.try_recv() {
            Ok(result) => match result {
                SessionFeedResult::Snapshot(snapshot) => {
                    if self
                        .session_feed_reload_retry_at
                        .is_some_and(|retry_at| std::time::Instant::now() < retry_at)
                    {
                        false
                    } else {
                        self.bind_in_flight_creation(&snapshot.contents.sessions);
                        let persisted_ordering = crate::session::load_workspace_ordering()
                            .map(|ordering| ordering.order)
                            .unwrap_or_default();
                        let unknown_row = snapshot.contents.sessions.iter().any(|row| {
                            self.active_profile
                                .as_ref()
                                .is_none_or(|active| *active == row.profile)
                                && !self.instances.contains_key(&row.id)
                                && self.in_flight_creation_id() != Some(row.id.as_str())
                                && !self.pending_creation.as_ref().is_some_and(|pending| {
                                    pending.confirmation.is_none()
                                        && row.idempotency_key.as_deref()
                                            == Some(pending.request_key.as_str())
                                })
                        });
                        let refresh_metadata = self.session_feed_reload_retry_at.is_some()
                            || unknown_row
                            || self
                                .snapshot_requires_storage_reload(&snapshot, &persisted_ordering);
                        let mut metadata_changed = false;
                        match self.apply_canonical_projection(&snapshot, refresh_metadata) {
                            Err(error) => {
                                self.session_feed_reload_retry_at = Some(
                                    std::time::Instant::now()
                                        + Self::CANONICAL_RELOAD_RETRY_INTERVAL,
                                );
                                tracing::warn!(target: "tui.session_feed", %error,
                                    "staging a canonical runtime revision failed");
                                self.info_dialog = Some(InfoDialog::new(
                                    "Runtime state not applied",
                                    &error.to_string(),
                                ));
                            }
                            Ok(()) => {
                                metadata_changed = true;
                                for row in &snapshot.contents.sessions {
                                    self.apply_daemon_status_update(row);
                                    if !row.unread
                                        && self.manual_unread_hold.as_deref() == Some(&row.id)
                                    {
                                        self.manual_unread_hold = None;
                                    }
                                }
                                let ids: std::collections::HashSet<_> =
                                    self.instances.keys().map(String::as_str).collect();
                                self.structured_pending_approvals
                                    .retain(|id, _| ids.contains(id.as_str()));
                                self.rebuild_flat_items_keeping_cursor();
                                self.update_selected();
                                self.session_feed_reload_retry_at = None;
                                self.observed_workspace_ordering = persisted_ordering;
                                metadata_changed |=
                                    self.session_feed.mark_snapshot_applied(snapshot);
                                snapshot_applied = true;
                            }
                        }
                        metadata_changed |= self.set_sidebar_source(SidebarSource::Daemon, None);
                        metadata_changed
                    }
                }
                SessionFeedResult::Unavailable(reason) => {
                    self.session_feed_reload_retry_at = None;
                    self.set_sidebar_source(SidebarSource::Disconnected, Some(&reason))
                }
            },
            Err(TryRecvError::Empty) => false,
            Err(TryRecvError::Disconnected) => false,
        };
        if !self.session_feed.native_interaction_available() {
            self.cancel_native_attachment();
            if snapshot_applied || self.live_send.is_some() {
                self.teardown_live_send();
            }
            self.pending_paste = None;
        }
        let archive_cursor_changed = snapshot_applied && self.apply_pending_archive_cursor();
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
        let outcomes_changed = self.apply_runtime_outcomes();
        let pending_after =
            self.session_feed.any_pending() || self.pending_namespace_intent.is_some();
        updated
            || command_error
            || archive_cursor_changed
            || outcomes_changed
            || pending_before != pending_after
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
    fn record_runtime_message(&mut self, message: String) {
        tracing::info!(target: "tui.runtime_outcome", message, "Runtime domain outcome");
        self.runtime_outcome_messages.push(message);
    }

    fn record_purge_outcome(&mut self, id: &str, outcome: crate::daemon::PurgeOutcome) {
        match outcome {
            crate::daemon::PurgeOutcome::Deleted {
                messages,
                cleanup_errors,
            } => {
                for message in messages {
                    self.record_runtime_message(format!("{id}: {message}"));
                }
                for error in cleanup_errors {
                    self.record_runtime_message(format!(
                        "{id}: Session deletion committed, but cleanup is incomplete: {error}"
                    ));
                }
            }
            crate::daemon::PurgeOutcome::Kept {
                messages,
                teardown_started,
            } => {
                self.record_runtime_message(format!(
                    "{id}: Session kept{}.",
                    if teardown_started {
                        " after runtime teardown began"
                    } else {
                        " without runtime teardown"
                    }
                ));
                for message in messages {
                    self.record_runtime_message(format!("{id}: {message}"));
                }
            }
        }
    }

    fn apply_runtime_outcomes(&mut self) -> bool {
        use crate::daemon::{NamespaceOutcome, ReorderOutcome};
        use crate::tui::session_feed::{CommandFailure, SessionCommandOutcome};
        let row_outcomes = self.session_feed.drain_session_outcomes();
        let namespace = self.session_feed.drain_namespace_result();
        let mut changed = !row_outcomes.is_empty() || namespace.is_some();
        for (id, outcome) in row_outcomes {
            match outcome {
                SessionCommandOutcome::Trashed(receipt) => {
                    if let crate::daemon::TrashRelocationOutcome::Failed { reason } =
                        receipt.outcome.relocation
                    {
                        self.record_runtime_message(format!("{id}: Session is in Trash, but its worktree relocation is incomplete: {reason}"));
                    }
                }
                SessionCommandOutcome::Purged(receipt) => {
                    self.record_purge_outcome(&id, receipt.outcome)
                }
                SessionCommandOutcome::Renamed(receipt) => {
                    for warning in receipt.outcome.warnings {
                        self.record_runtime_message(format!("{id}: {warning}"));
                    }
                }
                SessionCommandOutcome::WorktreeEdited(receipt) => {
                    for warning in receipt.outcome.warnings {
                        self.record_runtime_message(format!("{id}: {warning}"));
                    }
                }
                SessionCommandOutcome::ProjectAttached(receipt) => {
                    if self
                        .info_dialog
                        .as_ref()
                        .is_some_and(|dialog| dialog.title() == "Attaching Project")
                    {
                        self.info_dialog = None;
                    }
                    let attached = receipt.outcome.attached;
                    self.record_runtime_message(format!(
                        "{id}: Attached '{}' at '{}' on branch '{}'{}{}.",
                        attached.name,
                        attached.worktree_path,
                        attached.branch,
                        if attached.branch_created {
                            " (new branch)"
                        } else {
                            " (existing branch)"
                        },
                        attached
                            .moved_to
                            .map(|path| format!("; session moved to '{path}'"))
                            .unwrap_or_default()
                    ));
                    for warning in receipt.outcome.warnings {
                        self.record_runtime_message(format!("{id}: {warning}"));
                    }
                    self.record_runtime_message(match receipt.outcome.worker {
                        crate::daemon::AttachedWorkerOutcome::Restarted => format!("{id}: The agent restarted and sees the attached project."),
                        crate::daemon::AttachedWorkerOutcome::NotRunning => format!("{id}: The stopped agent will see the project on its next start."),
                        crate::daemon::AttachedWorkerOutcome::RestartFailed { message } => format!("{id}: Project attachment committed, but the agent restart failed: {message}"),
                    });
                }
                SessionCommandOutcome::Restored(receipt) => match receipt.outcome {
                    crate::daemon::RestoreOutcome::Restored => {
                        if self.selected_session.as_deref() == Some(id.as_str()) {
                            self.select_session_by_id(&id);
                        }
                    }
                    crate::daemon::RestoreOutcome::AlreadyRestored => self.record_runtime_message(
                        format!("{id}: already restored; canonical state retained"),
                    ),
                },
            }
        }
        if let Some(result) = namespace {
            let intent = self.pending_namespace_intent.take();
            match result {
                Err(CommandFailure::Rejected(message)) => {
                    self.record_runtime_message(format!("Namespace change rejected: {message}"))
                }
                Err(CommandFailure::Unknown(message)) => {
                    self.namespace_unknown_message = Some(message);
                    self.open_namespace_indeterminate_dialog();
                }
                Ok(receipt) => match receipt.outcome {
                    NamespaceOutcome::Committed => match intent {
                        Some(NamespaceIntent::ProfileCreated(name)) => {
                            if let Err(error) = self.switch_profile(Some(name)) {
                                self.record_runtime_message(format!("Profile created, but changing the displayed profile failed: {error}"));
                            }
                        }
                        Some(NamespaceIntent::ProfileDeleted(name)) => {
                            self.rewire_after_profile_delete(&name);
                            self.show_profile_picker();
                        }
                        _ => {}
                    },
                    NamespaceOutcome::DeletedGroup(outcome) => {
                        if !outcome.group_removed {
                            self.record_runtime_message(
                                "The group remains in the canonical namespace.".to_owned(),
                            );
                        }
                        for session in outcome.sessions {
                            self.record_purge_outcome(&session.id, session.outcome);
                        }
                        for failure in outcome.failures {
                            self.record_runtime_message(format!(
                                "{}: {}",
                                failure.id, failure.message
                            ));
                        }
                    }
                    NamespaceOutcome::Reordered(outcome) => match outcome {
                        ReorderOutcome::Moved { destination } => {
                            if let Some(NamespaceIntent::ReorderSession { id, .. }) = intent {
                                self.follow_committed_reorder(&id, destination.as_ref());
                            } else {
                                self.rebuild_flat_items_keeping_cursor();
                                self.update_selected();
                            }
                        }
                        ReorderOutcome::AtEdge => {
                            if let Some(NamespaceIntent::ReorderSession {
                                id,
                                profile,
                                source_group,
                                delta,
                                crossing: false,
                                continuation_destination,
                            }) = intent
                            {
                                // A keystroke owns both steps, but each receipt has its
                                // own applied fence. Never retarget to the new cursor.
                                if self.instances.get(&id).is_some_and(|row| {
                                    row.source_profile == profile
                                        && row.group_path == source_group
                                        && !row.is_archived()
                                        && !row.is_trashed()
                                }) {
                                    if let Some(destination) = continuation_destination {
                                        if destination.is_empty()
                                            || self
                                                .group_trees
                                                .get(&profile)
                                                .is_some_and(|tree| tree.group_exists(&destination))
                                        {
                                            if let Err(error) = self.submit_session_reorder(
                                                &id,
                                                delta,
                                                Some(destination),
                                            ) {
                                                self.record_runtime_message(format!(
                                                    "Could not cross the group boundary: {error}"
                                                ));
                                            }
                                        } else {
                                            let _ = self.refresh_after_stale_move();
                                        }
                                    }
                                } else {
                                    let _ = self.refresh_after_stale_move();
                                }
                            }
                        }
                        ReorderOutcome::Stale => {
                            let _ = self.refresh_after_stale_move();
                        }
                    },
                },
            }
        }
        if self.info_dialog.is_none() && !self.runtime_outcome_messages.is_empty() {
            let messages = std::mem::take(&mut self.runtime_outcome_messages);
            self.info_dialog = Some(InfoDialog::sized_to_fit(
                "Runtime change",
                &messages.join("\n"),
            ));
            changed = true;
        }
        changed
    }

    pub(super) fn open_namespace_indeterminate_dialog(&mut self) {
        if !self.session_feed.namespace_indeterminate() {
            return;
        }
        let message = self
            .namespace_unknown_message
            .as_deref()
            .unwrap_or("The admitted namespace operation was interrupted.");
        self.confirm_dialog = Some(ConfirmDialog::new("Resolve Unknown Namespace Outcome", &format!("{message}\n\nReview the current canonical sessions, groups and profiles. Resolving acknowledges uncertainty; it does not retry the operation. Keep Blocked leaves the namespace quarantined. Reopen with Ctrl+K: Resolve unknown runtime change."), "resolve_namespace_indeterminate").buttons("Acknowledge", "Keep Blocked"));
    }

    /// Open the oldest unresolved outcome without replaying its mutation.
    pub(super) fn promote_next_indeterminate(&mut self) {
        if self.session_feed.namespace_indeterminate() {
            self.open_namespace_indeterminate_dialog();
            return;
        }
        let Some((id, message)) = self.pending_indeterminate_queue.first().cloned() else {
            return;
        };
        self.open_indeterminate_dialog(&id, &message);
    }
}
