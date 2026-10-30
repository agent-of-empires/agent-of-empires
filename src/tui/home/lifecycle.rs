//! Building the view, and reloading it when storage, profiles, or the
//! watched config change underneath.

use super::*;

impl HomeView {
    pub fn new(
        active_profile: Option<String>,
        available_tools: AvailableTools,
        file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) -> anyhow::Result<Self> {
        let storages = HashMap::new();
        let all_instances = Vec::new();
        let group_trees = HashMap::new();
        let legacy_duplicate_reports = Vec::new();

        let user_config = load_config().ok().flatten();
        let config_profile = active_profile.clone().or_else(|| {
            user_config
                .as_ref()
                .map(|config| config.default_profile.clone())
                .filter(|profile| !profile.is_empty())
                .or_else(|| crate::session::list_profiles().ok()?.into_iter().next())
        });
        let resolved = config_profile
            .as_deref()
            .map(resolve_config_or_warn)
            .unwrap_or_else(crate::session::Config::load_or_warn);
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
        crate::session::set_preview_link_prefixes(&resolved.session.preview_link_prefixes);
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
            link_policy_dirty: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
            session_feed_failed_cursor: None,
            canonical_metadata_lag: None,
            pending_created_attach: None,
            retained_reveal: None,
            runtime_diagnostics: Vec::new(),
            group_trees,
            legacy_duplicate_reports,
            flat_items: Vec::new(),
            cursor: 0,
            selected_session: None,
            selected_group: None,
            selected_group_profile: None,
            view_mode,
            show_last_prompt: false,
            last_prompt_cache: None,
            last_prompt_slot: last_prompt::new_slot(),
            last_prompt_in_flight: false,
            sort_order,
            group_by,
            row_tag_mode: resolved.session.row_tag,
            show_activity_age: resolved.session.show_activity_age,
            sidebar_position: resolved.session.sidebar_position,
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
            connections_view: None,
            pending_dashboard_launch: None,
            dashboard_launch: None,
            update_confirm_dialog: None,
            telemetry_consent_dialog: None,
            tips_dialog: None,
            tips_unseen,
            pending_tip_pop: None,
            tips_badge_rect: None,
            tips_badge_hovered: false,
            send_message_dialog: None,
            permission_response_dialog: None,
            agent_question_dialog: None,
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
            confirm_checked: Vec::new(),
            pending_switch_view_session: None,
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
            show_shortcut_bar: resolved.session.show_shortcut_bar,
            preview_link_prefixes: resolved.session.preview_link_prefixes.clone(),
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
            runtime_failure_reason: None,
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
            serve_running: false,
            serve_probe: crate::tui::worker::Worker::spawn("aoe-serve-probe", |()| {
                crate::cli::serve::web_daemon_pid().is_some()
            }),
            serve_probe_in_flight: false,
            idle_decay_window,
            settings_view: None,
            settings_close_confirm: false,
            diff_view: None,
            list_width: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_list_width)
                .unwrap_or(35),
            divider_col: None,
            list_height: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_list_height),
            divider_row: None,
            main_area_width: 0,
            main_area_height: 0,
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
            split_terminal: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_split_terminal)
                .unwrap_or(false),
            split_capture_worker: None,
            split_capture_target: None,
            archived_section_collapsed: user_config
                .as_ref()
                .and_then(|c| c.app_state.archived_section_collapsed)
                .unwrap_or(true),
            trashed_section_collapsed: true,
            hide_stopped_in_groups: false,
            group_totals: HashMap::new(),
            opener_exits: Vec::new(),
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
        view.refresh_flat_items();
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
        view.refresh_shortcut_bar_tip();
        Ok(view)
    }

    #[cfg(test)]
    pub(crate) fn new_for_test(
        active_profile: Option<String>,
        available_tools: AvailableTools,
        file_watch: std::sync::Arc<crate::file_watch::FileWatchService>,
    ) -> anyhow::Result<Self> {
        let mut view = Self::new(active_profile, available_tools, file_watch)?;
        view.load_storage_projection()?;
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
        self.refresh_shortcut_bar_tip();
        self.reload_storage_only()
    }

    pub(in crate::tui) fn reload_storage_only(&mut self) -> anyhow::Result<()> {
        self.apply_session_feed();
        if let Some(snapshot) = self.session_feed.applied_snapshot() {
            if self
                .session_feed_failed_cursor
                .as_ref()
                .is_some_and(|failed| {
                    failed.epoch == snapshot.cursor.epoch
                        && failed.revision > snapshot.cursor.revision
                })
            {
                return Ok(());
            }
            match self.apply_canonical_projection(&snapshot, true) {
                Ok(()) => {
                    self.session_feed_reload_retry_at = None;
                    self.session_feed_failed_cursor = None;
                    self.canonical_metadata_lag = None;
                    self.reload_failure_state.record_canonical(&Ok(()));
                    self.refresh_projection_presentation();
                }
                Err(error) => self.record_canonical_projection_failure(&snapshot.cursor, &error),
            }
            Ok(())
        } else if self.sidebar_source == crate::tui::session_feed::SidebarSource::Daemon {
            Ok(())
        } else {
            self.load_storage_projection()
        }
    }

    /// Load a read-only preview before an acknowledged runtime frame exists.
    pub(super) fn load_storage_projection(&mut self) -> anyhow::Result<()> {
        let current_profiles = crate::session::list_profiles()?;
        let scope: Vec<_> = current_profiles
            .iter()
            .filter(|name| {
                self.active_profile
                    .as_ref()
                    .is_none_or(|active| active == *name)
            })
            .cloned()
            .collect();
        let mut opened = HashMap::new();
        for profile in &scope {
            if !self.storages.contains_key(profile) {
                opened.insert(
                    profile.clone(),
                    Storage::open(profile, self.file_watch.clone())?,
                );
            }
        }
        let mut loads = Vec::new();
        for profile in &scope {
            let storage = self
                .storages
                .get(profile)
                .or_else(|| opened.get(profile))
                .expect("profile store staged");
            let (mut rows, groups) = storage.load_complete_with_groups()?;
            for row in &mut rows {
                row.source_profile.clone_from(profile);
                if let Some(previous) = self.instances.get(&row.id) {
                    row.merge_runtime_from_reload(previous);
                }
            }
            loads.push((profile.clone(), rows, groups));
        }
        let load_refs: Vec<_> = loads
            .iter()
            .map(|(name, rows, _)| (name.as_str(), rows.as_slice()))
            .collect();
        let stores: Vec<_> = scope
            .iter()
            .map(|profile| {
                (
                    profile.as_str(),
                    self.storages
                        .get(profile)
                        .or_else(|| opened.get(profile))
                        .unwrap(),
                )
            })
            .collect();
        let reports = crate::session::duplicate_reports(&load_refs, &stores);
        let mut trees = HashMap::new();
        let mut rows = Vec::new();
        for (profile, instances, groups) in loads {
            trees.insert(profile, GroupTree::new_with_groups(&instances, &groups));
            rows.extend(instances);
        }
        let mut next = Self::build_instances_map(rows);
        if let Some(stub) = self
            .creating_stub_id
            .as_ref()
            .and_then(|id| self.instances.get(id))
        {
            next.insert(stub.id.clone(), stub.clone());
        }
        if let Some(id) = self
            .pending_creation
            .as_ref()
            .filter(|pending| pending.confirmation.is_none())
            .and_then(|pending| pending.daemon_id.as_deref())
        {
            next.shift_remove(id);
        }
        self.instances = next;
        self.group_trees = trees;
        self.storages.extend(opened);
        self.storages.retain(|profile, _| scope.contains(profile));
        log_legacy_duplicates_once(&reports);
        self.legacy_duplicate_reports = reports;
        self.rewire_config_subscriptions(&current_profiles);
        self.rewire_disk_subscriptions(&scope);
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
            self.end_live_send_on_projection_drift(&state);
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
        let info = self.info_dialog.take();
        let occupied = self.has_non_live_send_overlay();
        self.info_dialog = info;
        if occupied {
            return false;
        }
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
