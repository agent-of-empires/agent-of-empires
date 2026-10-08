//! Building the view, and reloading it when storage, profiles, or the
//! watched config change underneath.

use super::*;

impl HomeView {
    pub fn new(
        active_profile: Option<String>,
        available_tools: AvailableTools,
        file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) -> anyhow::Result<Self> {
        use crate::session::list_profiles;

        let mut storages = HashMap::new();
        let mut all_instances = Vec::new();
        let mut group_trees = HashMap::new();
        let mut profile_loads: Vec<(String, Vec<Instance>, Vec<Group>)> = Vec::new();

        let profile_names = match &active_profile {
            Some(name) => vec![name.clone()],
            None => list_profiles()?.into_iter().collect(),
        };

        for profile_name in &profile_names {
            let storage = Storage::open(profile_name, file_watch.clone())?;
            let (mut instances, groups) = storage.load_complete_with_groups()?;
            for inst in &mut instances {
                inst.source_profile = profile_name.clone();
            }
            profile_loads.push((profile_name.clone(), instances, groups));
            storages.insert(profile_name.clone(), storage);
        }

        let legacy_duplicate_reports = {
            let loads_view: Vec<(&str, &[Instance])> = profile_loads
                .iter()
                .map(|(name, rows, _)| (name.as_str(), rows.as_slice()))
                .collect();
            let storages_view: Vec<(&str, &Storage)> = storages
                .iter()
                .map(|(name, storage)| (name.as_str(), storage))
                .collect();
            let reports = crate::session::duplicate_reports(&loads_view, &storages_view);
            log_legacy_duplicates_once(&reports);
            reports
        };
        for (profile_name, instances, groups) in &profile_loads {
            let tree = GroupTree::new_with_groups(instances, groups);
            group_trees.insert(profile_name.clone(), tree);
            all_instances.extend(instances.iter().cloned());
        }

        // In unified mode there is no single active profile, so config is
        // resolved from the user's default profile.
        let config_profile = active_profile
            .clone()
            .unwrap_or_else(crate::session::config::resolve_default_profile);
        let resolved = resolve_config_or_warn(&config_profile);
        let default_terminal_mode = match resolved.sandbox.default_terminal_mode {
            DefaultTerminalMode::Host => TerminalMode::Host,
            DefaultTerminalMode::Container => TerminalMode::Container,
        };
        let sound_config = resolved.sound.clone();
        let strict_hotkeys = resolved.session.strict_hotkeys;
        let confirm_before_quit = resolved.session.confirm_before_quit;
        let host_tab_title = resolved.session.host_tab_title;
        let idle_decay_window =
            crate::tui::styles::idle_decay_window(resolved.theme.idle_decay_minutes);
        crate::session::set_unread_enabled(resolved.session.unread_indicator);
        crate::session::set_favorites_first(resolved.session.favorites_first);
        let user_config = load_config().ok().flatten();
        let sort_order = user_config
            .as_ref()
            .and_then(|c| c.app_state.sort_order)
            .unwrap_or_default();
        // New users (who haven't dismissed the welcome screen) default to Project grouping
        // so they see the web dashboard's layout; existing users keep Manual unless they
        // toggle with `g`.
        let is_new_user = user_config
            .as_ref()
            .is_none_or(|c| !c.app_state.has_seen_welcome);
        let default_group_by = if is_new_user {
            GroupByMode::Project
        } else {
            GroupByMode::Manual
        };
        let group_by = user_config
            .as_ref()
            .and_then(|c| c.app_state.group_by)
            .unwrap_or(default_group_by);
        let tips_unseen = user_config.as_ref().map_or_else(
            || tips_unseen_count(&crate::session::Config::default()),
            tips_unseen_count,
        );
        let view_mode = ViewMode::default();

        let disk_watch = DiskWatchState {
            dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            handles: HashMap::new(),
        };

        let config_watch = ConfigWatchState {
            dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            handles: HashMap::new(),
        };

        let mut view = Self {
            storages,
            active_profile,
            instances: Self::build_instances_map(all_instances),
            observed_workspace_ordering: crate::session::load_workspace_ordering()
                .map(|ordering| ordering.order)
                .unwrap_or_default(),
            session_feed_reload_retry_at: None,
            group_trees,
            legacy_duplicate_reports,
            flat_items: Vec::new(),
            cursor: 0,
            selected_session: None,
            selected_group: None,
            selected_group_profile: None,
            view_mode,
            sort_order,
            group_by,
            row_tag_mode: resolved.session.row_tag,
            show_activity_age: resolved.session.show_activity_age,
            sidebar_position: user_config
                .as_ref()
                .map(|c| c.session.sidebar_position)
                .unwrap_or_default(),
            agent_clipboard_forward: resolved.tmux.clipboard
                != crate::session::config::TmuxSettingMode::Disabled,
            hyperlink_cells: crate::tui::hyperlink::SharedHyperlinks::default(),
            vt_live_enabled: resolved.tmux.vt_live,
            profile_default_attach_mode: resolved.session.default_attach_mode,
            project_group_collapsed: user_config
                .as_ref()
                .map(|c| {
                    c.app_state
                        .project_group_collapsed
                        .iter()
                        .map(|path| (path.clone(), true))
                        .collect()
                })
                .unwrap_or_default(),
            org_group_collapsed: user_config
                .as_ref()
                .map(|c| {
                    c.app_state
                        .org_group_collapsed
                        .iter()
                        .map(|path| (path.clone(), true))
                        .collect()
                })
                .unwrap_or_default(),
            remote_owner_cache: std::cell::RefCell::new(HashMap::new()),
            registered_projects: Vec::new(),
            show_help: false,
            help_scroll: 0,
            new_dialog: None,
            confirm_dialog: None,
            unified_delete_dialog: None,
            group_delete_options_dialog: None,
            rename_dialog: None,
            worktree_name_dialog: None,
            restart_dialog: None,
            context_menu: None,
            group_rename_context: None,
            repo_trust_dialog: None,
            pending_repo_trust_data: None,
            pending_repo_trust_fingerprint: None,
            hooks_install_dialog: None,
            pending_hooks_install_data: None,
            volume_ignores_glob_dialog: None,
            pending_volume_ignores_glob_data: None,
            intro_dialog: None,
            pending_intro_theme: None,
            no_agents_dialog: None,
            changelog_dialog: None,
            info_dialog: None,
            pending_indeterminate_resolution: None,
            pending_indeterminate_queue: Vec::new(),
            snooze_duration_dialog: None,
            pending_snooze_session: None,
            profile_picker_dialog: None,
            group_picker_dialog: None,
            sort_picker_dialog: None,
            attach_project_dialog: None,
            project_session_picker_dialog: None,
            projects_dialog: None,
            plugin_manager_dialog: None,
            skills_manager_dialog: None,
            command_palette: None,
            serve_view: None,
            update_confirm_dialog: None,
            telemetry_consent_dialog: None,
            tips_dialog: None,
            tips_unseen,
            pending_tip_pop: None,
            tips_badge_rect: None,
            tips_badge_hovered: false,
            send_message_dialog: None,
            permission_response_dialog: None,
            pending_permission_response: None,
            pending_send_session: None,
            pending_send_target: live_send::LiveSendTarget::Agent,
            pending_live_send_target: live_send::LiveSendTarget::Agent,
            pending_native_attachment: None,
            live_send: None,
            live_send_worker: None,
            preview_capture_worker: None,
            preview_capture_target: None,
            preview_worker_pulse: None,
            preview_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            live_send_last_resize: None,
            live_send_resize_retry_at: None,
            live_send_pending_leader: false,
            hover_cell: None,
            status_flash: None,
            live_send_ctrl_c_flash_until: None,
            sidebar_collapsed: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_sidebar_collapsed)
                .unwrap_or(false),
            collapse_button_area: Rect::default(),
            expand_strip_area: Rect::default(),
            footer_buttons: Vec::new(),
            footer_hover: None,
            passive_pane_synced: std::collections::HashMap::new(),
            passive_pane_declined: std::collections::HashMap::new(),
            passive_pane_queued: std::collections::HashMap::new(),
            passive_fleet_armed: None,
            preview_pane_pending: None,
            pending_paste: None,
            pending_paste_for_structured_view: HashMap::new(),
            pending_attach_after_warning: None,
            pending_stop_session: None,
            pending_stop_auxiliary: None,
            pending_legacy_tool_preparation: None,
            pending_image_pull: None,
            pending_switch_view_session: None,
            pending_daemon_start_session: None,
            structured_preview: None,
            structured_preview_pending: false,
            pending_force_remove_session: None,
            pending_trash_session: None,
            pending_dialog_click_action: None,
            search_active: false,
            search_query: Input::default(),
            search_matches: Vec::new(),
            search_match_index: 0,
            available_tools,
            show_diagnostics: resolved.session.show_diagnostics_pane,
            metrics_poller: crate::tui::metrics_poller::MetricsPoller::new(),
            pending_metrics_refresh: false,
            metrics: crate::process::metrics::MetricsSnapshot::default(),
            system_health_open: false,
            system_health_scroll: 0,
            diagnostics_area: Rect::default(),
            diagnostics_hovered: false,
            system_health_tip_high_samples: 0,
            system_health_tip_earned: user_config
                .as_ref()
                .is_some_and(|config| config.app_state.system_health_tip_earned),
            system_health_discovered: user_config
                .as_ref()
                .is_some_and(|config| config.app_state.used_system_health),
            structured_pending_approvals: HashMap::new(),
            structured_approval_poller: crate::tui::approval_poller::StructuredApprovalPoller::new(
            ),
            session_feed: crate::tui::session_feed::SessionFeed::new(),
            project_registry_authoritative: false,
            pending_namespace_intent: None,
            namespace_unknown_message: None,
            runtime_outcome_messages: Vec::new(),
            sidebar_source: crate::tui::session_feed::SidebarSource::Disconnected,
            runtime_failure_message: None,
            restart_in_flight: std::collections::HashSet::new(),
            store_move_poller: crate::tui::store_move_poller::StoreMovePoller::new(),
            store_move_in_flight: None,
            store_move_bypass: None,
            pending_creation: None,
            structured_transcript_painted: false,
            creating_hook_progress: HashMap::new(),
            creating_stub_id: None,
            pending_archive_cursor: None,
            preview_cache: PreviewCache::default(),
            preview_timings: PreviewTimings::default(),
            terminal_preview_cache: PreviewCache::default(),
            container_terminal_preview_cache: PreviewCache::default(),
            tool_preview_cache: PreviewCache::default(),
            preview_scroll_offset: 0,
            preview_text_view: PreviewTextView::default(),
            preview_area: Rect::default(),
            preview_pane_area: Rect::default(),
            preview_visible_rows: 0,
            preview_outer_area: Rect::default(),
            diff_area: Rect::default(),
            list_area: Rect::default(),
            list_inner_area: Rect::default(),
            shelf_inner_area: Rect::default(),
            mouse_pos: None,
            last_click: None,
            last_preview_click: None,
            unread_dwell: None,
            manual_unread_hold: None,
            terminal_modes: HashMap::new(),
            default_terminal_mode,
            sound_config,
            strict_hotkeys,
            confirm_before_quit,
            host_tab_title,
            active_tui_count: 1,
            idle_decay_window,
            settings_view: None,
            settings_close_confirm: false,
            diff_view: None,
            list_width: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_list_width)
                .unwrap_or(35),
            divider_col: None,
            main_area_width: 0,
            drag_state: None,
            mouse_forward_btn: None,
            hover_forward_cell: None,
            preview_drag_pos: None,
            preview_autoscroll_at: None,
            preview_selection: None,
            preview_copy_pending: false,
            preview_copy_text: None,
            show_preview_info: user_config
                .as_ref()
                .and_then(|c| c.app_state.show_preview_info)
                .unwrap_or(true),
            archived_section_collapsed: user_config
                .as_ref()
                .and_then(|c| c.app_state.archived_section_collapsed)
                .unwrap_or(true),
            trashed_section_collapsed: true,
            restart_cooldown_at: std::collections::HashMap::new(),
            tool_configs: user_config
                .as_ref()
                .map(|c| c.tools.clone())
                .unwrap_or_default(),
            tool_hotkey_cache: Vec::new(),
            tool_picker_dialog: None,
            file_watch,
            disk_watch,
            config_watch,
            watcher_config_refresh_count: std::sync::atomic::AtomicU64::new(0),
            reload_failure_state: ReloadFailureState::default(),
            // App::new loads the boot theme; no startup stash from HomeView.
            pending_watcher_theme: None,
        };

        view.tool_hotkey_cache = input::build_tool_hotkey_cache(&view.tool_configs);
        let hotkey_warnings = input::validate_tool_hotkeys(&view.tool_configs);
        if !hotkey_warnings.is_empty() && view.info_dialog.is_none() {
            view.info_dialog = Some(InfoDialog::new(
                "Tool hotkey config errors",
                &hotkey_warnings.join("\n"),
            ));
        }

        view.refresh_registered_projects();
        view.flat_items = view.build_flat_items();
        view.update_selected();
        // Stable subscription order keeps the shared watcher target deterministic.
        let mut initial_disk_profiles: Vec<String> = view.storages.keys().cloned().collect();
        initial_disk_profiles.sort();
        view.rewire_disk_subscriptions(&initial_disk_profiles);
        // Watch all profile configs even in a filtered view.
        let initial_config_profiles: Vec<String> = match crate::session::list_profiles() {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    error = %e,
                    "list_profiles failed at startup; falling back to loaded storages for config wiring"
                );
                initial_disk_profiles.clone()
            }
        };
        view.rewire_config_subscriptions(&initial_config_profiles);
        Ok(view)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        active_profile: Option<String>,
        available_tools: AvailableTools,
        file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) -> anyhow::Result<Self> {
        let mut view = Self::new(active_profile, available_tools, file_watch)?;
        let default_profile = view
            .active_profile
            .clone()
            .unwrap_or_else(crate::session::config::resolve_default_profile);
        let snapshot = super::tests::fixture_snapshot(
            view.instances()
                .map(|instance| crate::daemon::SessionResponse::from_instance(instance, false))
                .collect(),
            &default_profile,
            "test",
            0,
        );
        let cursor = snapshot.cursor.clone();
        view.session_feed
            .publish_for_test(crate::tui::session_feed::SessionFeedResult::Snapshot(
                std::sync::Arc::new(snapshot),
            ));
        view.apply_session_feed();
        anyhow::ensure!(
            view.session_feed.receipt_applied(&cursor),
            "fixture snapshot was not applied"
        );
        Ok(view)
    }

    /// Refresh read-only presentation metadata, retaining the last acknowledged canonical projection.
    pub fn reload(&mut self) -> anyhow::Result<()> {
        self.reload_storage_only()
    }

    pub(in crate::tui) fn reload_storage_only(&mut self) -> anyhow::Result<()> {
        if let Some(snapshot) = self.session_feed.applied_snapshot() {
            self.apply_canonical_projection(&snapshot, true)?;
            self.refresh_projection_presentation();
            Ok(())
        } else {
            self.load_storage_projection()
        }
    }

    /// Load a read-only preview before an acknowledged runtime frame exists.
    pub(super) fn load_storage_projection(&mut self) -> anyhow::Result<()> {
        use crate::session::list_profiles;

        let mut all_instances = Vec::new();

        let current_profiles = match list_profiles() {
            Ok(profiles) => profiles,
            Err(error) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    error = %error,
                    "list_profiles failed during reload_storage_only; reusing loaded storages for watcher rewires"
                );
                let mut keys: Vec<String> = self.storages.keys().cloned().collect();
                keys.sort();
                keys
            }
        };

        // Watch all profile configs, but only loaded profile data.
        self.rewire_config_subscriptions(&current_profiles);
        if self.active_profile.is_some() {
            let mut active_only: Vec<String> = self.storages.keys().cloned().collect();
            active_only.sort();
            self.rewire_disk_subscriptions(&active_only);
        } else {
            self.rewire_disk_subscriptions(&current_profiles);
        }

        // Storage rebuild is unified mode only: single-profile mode keeps the scope set at
        // startup, with only the active profile in memory.
        if self
            .active_profile
            .as_ref()
            .is_some_and(|profile| !current_profiles.contains(profile))
        {
            self.storages.clear();
        }
        if self.active_profile.is_none() {
            for name in &current_profiles {
                if !self.storages.contains_key(name) {
                    self.storages
                        .insert(name.clone(), Storage::open(name, self.file_watch.clone())?);
                }
            }
            self.storages.retain(|k, _| current_profiles.contains(k));
        }

        // Read-only diagnostic collection; repairs are daemon-owned.
        type ProfileLoads = Vec<(String, Vec<Instance>, Vec<Group>)>;
        let collect_loads = |storages: &HashMap<String, Storage>,
                             prev: &indexmap::IndexMap<String, Instance>|
         -> anyhow::Result<ProfileLoads> {
            let mut loads = Vec::new();
            for (profile_name, storage) in storages {
                let (mut instances, groups) = storage.load_complete_with_groups()?;
                for inst in &mut instances {
                    inst.source_profile = profile_name.clone();
                    if let Some(previous) = prev.get(&inst.id) {
                        // Field-ownership rules (generation-governed vs
                        // runtime-only) live on merge_runtime_from_reload.
                        inst.merge_runtime_from_reload(previous);
                    }
                }
                loads.push((profile_name.clone(), instances, groups));
            }
            Ok(loads)
        };
        let loads = collect_loads(&self.storages, &self.instances)?;
        let loads_view: Vec<(&str, &[Instance])> = loads
            .iter()
            .map(|(name, rows, _)| (name.as_str(), rows.as_slice()))
            .collect();
        let storages_view: Vec<(&str, &Storage)> = self
            .storages
            .iter()
            .map(|(name, storage)| (name.as_str(), storage))
            .collect();
        let reports = crate::session::duplicate_reports(&loads_view, &storages_view);
        log_legacy_duplicates_once(&reports);
        self.legacy_duplicate_reports = reports;

        for (profile_name, instances, groups) in &loads {
            let new_tree = GroupTree::new_with_groups(instances, groups);
            self.group_trees.insert(profile_name.clone(), new_tree);
            all_instances.extend(instances.iter().cloned());
        }

        // Remove trees for profiles that no longer exist
        let storage_keys: Vec<String> = self.storages.keys().cloned().collect();
        self.group_trees.retain(|k, _| storage_keys.contains(k));

        // Preserve the display-only placeholder across a canonical row reload.
        let creating_stub_snapshot: Option<Instance> = self
            .creating_stub_id
            .as_ref()
            .and_then(|id| self.instances.get(id).cloned());

        self.instances = Self::build_instances_map(all_instances);

        if let Some(stub) = creating_stub_snapshot {
            self.instances.entry(stub.id.clone()).or_insert(stub);
        }
        // The creation in flight is displayed by its placeholder alone; its own
        // reservation row stays out of the model until the daemon commits.
        if let Some(id) = self
            .pending_creation
            .as_ref()
            .filter(|pending| pending.confirmation.is_none())
            .and_then(|pending| pending.daemon_id.as_deref())
        {
            self.instances.shift_remove(id);
        }

        // Refresh the project registry so project view's empty pinned headers
        // and pin indicators reflect the current on-disk registry.
        self.refresh_registered_projects();

        self.refresh_projection_presentation();
        Ok(())
    }

    pub(super) fn refresh_projection_presentation(&mut self) {
        self.remote_owner_cache.borrow_mut().clear();

        let prev_selected_session = self.selected_session.clone();

        self.rebuild_flat_items_keeping_cursor();

        // Storage rebuilds and search re-scoring must not move the live-send
        // selection. Teardown reconciles it with the latest projection.
        let preserve_live_selection = self.live_send.as_ref().is_some_and(|state| {
            prev_selected_session.as_deref() == Some(state.session_id.as_str())
        });

        if self.search_active && !self.search_query.value().is_empty() {
            if preserve_live_selection {
                self.refresh_search_matches();
            } else {
                self.update_search();
            }
        } else if !self.search_matches.is_empty() {
            // Recalculate match indices without moving the cursor
            self.refresh_search_matches();
        }

        if !preserve_live_selection {
            self.update_selected();
        }
        if let Some(state) = self.live_send.clone() {
            self.end_live_send_on_drift(&state);
        }
    }

    /// Forwards to [`DiskWatchState::rewire`], lending it the
    /// `file_watch` Arc and `reload_failure_state` owned by `HomeView`.
    pub(in crate::tui) fn rewire_disk_subscriptions(&mut self, current: &[String]) {
        self.disk_watch
            .rewire(&self.file_watch, current, &mut self.reload_failure_state);
    }

    /// Forwards to [`ConfigWatchState::rewire`], lending it the
    /// `file_watch` Arc and `reload_failure_state` owned by `HomeView`.
    pub(in crate::tui) fn rewire_config_subscriptions(&mut self, current: &[String]) {
        self.config_watch
            .rewire(&self.file_watch, current, &mut self.reload_failure_state);
    }

    /// Rewire disk + config subscriptions after a successful profile
    /// delete. Surfaces a `Watcher Warning` dialog when
    /// `list_profiles()` cannot enumerate profiles, since the dialog
    /// is the only user-facing signal the delete path has; the next
    /// successful reload repairs watcher state.
    pub(in crate::tui) fn rewire_after_profile_delete(&mut self, profile_name: &str) {
        match crate::session::list_profiles() {
            Ok(profiles) => {
                let disk_targets: Vec<String> = if self.active_profile.is_some() {
                    let mut keys: Vec<String> = self.storages.keys().cloned().collect();
                    keys.sort();
                    keys
                } else {
                    profiles.clone()
                };
                self.rewire_disk_subscriptions(&disk_targets);
                self.rewire_config_subscriptions(&profiles);
            }
            Err(e) => {
                tracing::warn!(
                    target: "tui.file_watch",
                    profile = %profile_name,
                    op = "delete_profile",
                    error = %e,
                    "list_profiles failed during rewire after profile delete; watcher state will repair on next reload"
                );
                if self.info_dialog.is_none() {
                    self.info_dialog = Some(InfoDialog::new(
                        WATCHER_WARNING_TITLE,
                        &format!(
                            "Profile '{}' was deleted but the watcher rewire could not enumerate profiles: {}\n\nThe next successful reload will repair watcher state.",
                            profile_name, e
                        ),
                    ));
                }
            }
        }
    }

    /// Open or refresh the `Reload Failed` dialog from the current
    /// `reload_failure_state`. Returns `true` when the dialog was
    /// opened or its body refreshed in place so the caller can
    /// request a redraw.
    ///
    /// Three update paths converge here:
    /// * New burst presentation: `has_unacknowledged_failure()` is
    ///   true. The dialog opens (or re-opens) and the ack latch is
    ///   consumed.
    /// * Body refresh: when a `Reload Failed` dialog is on screen
    ///   and the ack latch is acknowledged, the body is rebuilt if
    ///   the failing-source set has shifted (partial recovery that
    ///   leaves at least one source still failing, or a new source
    ///   recorded for the same acknowledged burst). The ack latch
    ///   stays in place; the user is not re-notified for the same
    ///   ongoing burst.
    /// * No-op: live-send active, nothing failing, body unchanged, or an
    ///   unrelated dialog occupies the slot. While live or another dialog is
    ///   open, the ack latch stays armed so a later tick can present it.
    pub(in crate::tui) fn try_present_reload_failure_dialog(&mut self) -> bool {
        if self.live_send.is_some() || !self.reload_failure_state.has_any_failure() {
            return false;
        }
        let title = RELOAD_FAILED_TITLE;
        let occupied_by_other = self
            .info_dialog
            .as_ref()
            .is_some_and(|d| d.title() != title);
        if occupied_by_other {
            return false;
        }

        let needs_ack = self.reload_failure_state.has_unacknowledged_failure();
        let dialog_open = self
            .info_dialog
            .as_ref()
            .is_some_and(|d| d.title() == title);

        if !needs_ack && !dialog_open {
            return false;
        }

        let body = self.reload_failure_state.build_dialog_body();
        let body_matches = self
            .info_dialog
            .as_ref()
            .is_some_and(|d| d.message() == body);
        if !needs_ack && body_matches {
            return false;
        }

        self.info_dialog = Some(InfoDialog::sized_to_fit(title, &body));
        if needs_ack {
            self.reload_failure_state.acknowledge_dialog();
        }
        true
    }

    /// Recovery-edge cleanup: clear a stale `Reload Failed` dialog
    /// when every reload source returns to healthy. Returns `true`
    /// when the dialog was cleared so the caller can request a redraw.
    /// The `Watcher Warning` dialog raised by
    /// `rewire_after_profile_delete` is intentionally outside
    /// `reload_failure_state` and is left for the user to dismiss.
    pub(in crate::tui) fn try_clear_recovered_reload_dialog(&mut self) -> bool {
        if !self.reload_failure_state.has_any_failure()
            && self
                .info_dialog
                .as_ref()
                .is_some_and(|d| d.title() == RELOAD_FAILED_TITLE)
        {
            self.info_dialog = None;
            true
        } else {
            false
        }
    }
}
