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
            let storage = Storage::new(profile_name, file_watch.clone())?;
            let (mut instances, groups) = storage.load_with_groups()?;
            for inst in &mut instances {
                inst.source_profile = profile_name.clone();
            }
            // Clear expired lifecycle reservations in one write, under the same
            // per-instance flocks live transitions take. Acquired in sorted id
            // order, matching the only other multi-lock holder
            // (`Storage::move_instance_between_profiles`), which sorts too, so
            // the two cannot close a cycle.
            let ttl = crate::session::Instance::LIFECYCLE_RESERVATION_TTL;
            let now = chrono::Utc::now();
            let mut expired: Vec<String> = instances
                .iter()
                .filter(|instance| {
                    instance.lifecycle_reservation.is_some()
                        && !instance.has_fresh_lifecycle_reservation(now)
                })
                .map(|instance| instance.id.clone())
                .collect();
            if !expired.is_empty() {
                expired.sort();
                let mut locks = Vec::with_capacity(expired.len());
                expired.retain(|id| match storage.acquire_instance_lifecycle_lock(id) {
                    Ok(lock) => {
                        locks.push(lock);
                        true
                    }
                    Err(_) => false,
                });
                // `retain` can empty this when every lock failed; writing then
                // would rewrite sessions.json and notify subscribers for no
                // change at all.
                if !expired.is_empty() {
                    let cleared = storage.update(|disk, _groups| {
                        for id in &expired {
                            if let Some(stored) =
                                disk.iter_mut().find(|candidate| &candidate.id == id)
                            {
                                stored.clear_expired_lifecycle_reservation(ttl, now);
                            }
                        }
                        Ok(())
                    });
                    if cleared.is_ok() {
                        for instance in &mut instances {
                            if expired.contains(&instance.id) {
                                instance.clear_expired_lifecycle_reservation(ttl, now);
                            }
                        }
                    }
                }
            }
            profile_loads.push((profile_name.clone(), instances, groups));
            storages.insert(profile_name.clone(), storage);
        }

        // Duplicate detection across every loaded profile runs before any of
        // the loaded state is published (#3459). Journal-guided repairs fix
        // durable state under lock here, so a clean reload below publishes
        // exactly one row per session. Legacy ambiguities stay excluded.
        let legacy_duplicate_reports = {
            let loads_view: Vec<(&str, &[Instance])> = profile_loads
                .iter()
                .map(|(name, instances, _)| (name.as_str(), instances.as_slice()))
                .collect();
            let storages_view: Vec<(&str, &Storage)> = storages
                .iter()
                .map(|(name, storage)| (name.as_str(), storage))
                .collect();
            let outcome = crate::session::reconcile_profile_duplicates(&loads_view, &storages_view);
            if outcome.repaired {
                for (name, instances, groups) in &mut profile_loads {
                    let (mut fresh, fresh_groups) = storages[name].load_with_groups()?;
                    for inst in &mut fresh {
                        inst.source_profile = name.clone();
                    }
                    *instances = fresh;
                    *groups = fresh_groups;
                }
            }
            log_legacy_duplicates_once(&outcome.reports);
            outcome.reports
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
        let status_hook_configs = Self::load_status_hook_configs(Self::status_hook_profile_names(
            active_profile.as_deref(),
            &storages,
        ));
        let status_hook_config = status_hook_configs
            .get(&config_profile)
            .cloned()
            .unwrap_or_else(|| resolved.status_hooks.clone());
        let strict_hotkeys = resolved.session.strict_hotkeys;
        let confirm_before_quit = resolved.session.confirm_before_quit;
        let idle_decay_window =
            crate::tui::styles::idle_decay_window(resolved.theme.idle_decay_minutes);
        crate::session::set_unread_enabled(resolved.session.unread_indicator);
        crate::session::set_favorites_first(resolved.session.favorites_first);
        let user_config = load_config().ok().flatten();
        let sort_order = user_config
            .as_ref()
            .and_then(|c| c.app_state.sort_order)
            .unwrap_or_default();
        // New users (haven't dismissed the welcome screen) default to Project
        // grouping so they see the same layout as the web dashboard. Existing
        // users keep Manual (the existing behavior) unless they explicitly
        // toggle to Project with `g`.
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
            pending_deletions: HashMap::new(),
            pending_group_deletions: HashMap::new(),
            pending_added: HashMap::new(),
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
            agent_clipboard_forward: resolved.tmux.clipboard
                != crate::session::config::TmuxSettingMode::Disabled,
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
            hooks_install_dialog: None,
            pending_hooks_install_data: None,
            volume_ignores_glob_dialog: None,
            pending_volume_ignores_glob_data: None,
            intro_dialog: None,
            pending_intro_theme: None,
            no_agents_dialog: None,
            changelog_dialog: None,
            info_dialog: None,
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
            #[cfg(feature = "serve")]
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
            pending_permission_response_session: None,
            pending_send_session: None,
            pending_send_target: live_send::LiveSendTarget::Agent,
            pending_live_send_target: live_send::LiveSendTarget::Agent,
            live_send: None,
            live_send_worker: None,
            preview_capture_worker: None,
            preview_capture_target: None,
            preview_worker_pulse: None,
            preview_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            live_send_last_resize: None,
            live_send_resize_retry_at: None,
            live_send_pending_leader: false,
            live_send_ctrl_c_flash_until: None,
            sidebar_collapsed: user_config
                .as_ref()
                .and_then(|c| c.app_state.home_sidebar_collapsed)
                .unwrap_or(false),
            collapse_button_area: Rect::default(),
            expand_strip_area: Rect::default(),
            footer_buttons: Vec::new(),
            footer_hover: None,
            preview_pane_synced: None,
            preview_pane_pending: None,
            pending_paste: None,
            pending_attach_after_warning: None,
            pending_stop_session: None,
            pending_stop_terminal: None,
            pending_stop_tool: None,
            pending_image_pull: None,
            pending_switch_view_session: None,
            #[cfg(feature = "serve")]
            pending_daemon_start_session: None,
            #[cfg(feature = "serve")]
            structured_preview: None,
            #[cfg(feature = "serve")]
            structured_preview_pending: false,
            pending_force_remove_session: None,
            pending_trash_session: None,
            pending_dialog_click_action: None,
            search_active: false,
            search_query: Input::default(),
            search_matches: Vec::new(),
            search_match_index: 0,
            available_tools,
            status_poller: StatusPoller::new(),
            pending_status_refresh: false,
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
            #[cfg(feature = "serve")]
            daemon_status_poller: crate::tui::daemon_status_poller::DaemonStatusPoller::new(),
            #[cfg(feature = "serve")]
            pending_daemon_status_refresh: false,
            deletion_poller: DeletionPoller::new(),
            stop_poller: StopPoller::new(),
            trash_poller: crate::tui::trash_poller::TrashPoller::new(),
            reconcile_poller: crate::tui::reconcile_poller::ReconcilePoller::new(),
            startup_recovery_gate: None,
            pending_reconcile_reload: false,
            reconcile_reload_retry_at: None,
            restart_poller: RestartPoller::new(),
            restart_in_flight: std::collections::HashSet::new(),
            attach_project_poller: crate::tui::attach_project_poller::AttachProjectPoller::new(),
            attach_project_in_flight: std::collections::HashSet::new(),
            creation_poller: CreationPoller::new(),
            creation_cancelled: false,
            on_launch_hooks_ran: HashSet::new(),
            creating_hook_progress: HashMap::new(),
            creating_stub_id: None,
            creating_provisional_group_paths: HashSet::new(),
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
            status_hook_config,
            status_hook_configs,
            strict_hotkeys,
            confirm_before_quit,
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
            recovery_rx: None,
            recovery_lock: None,
            recovery_in_flight: std::collections::HashSet::new(),
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

        // Clean up orphaned Creating instances from a prior crash
        let orphan_ids: Vec<String> = view
            .instances
            .values()
            .filter(|i| i.status == crate::session::Status::Creating)
            .map(|i| i.id.clone())
            .collect();
        for id in &orphan_ids {
            view.remove_instance(id);
        }
        if !orphan_ids.is_empty() {
            tracing::info!(target: "tui.home", "Cleaned up {} orphaned creating sessions", orphan_ids.len());
            if let Err(e) = view.save() {
                tracing::warn!(target: "tui.home", "Failed to save view state: {e}");
            }
        }

        // Batch-sync instance IDs and captured session IDs to tmux hidden env
        // so that build_exclusion_set() on other AoE instances can see them.
        // One observation for both per-instance walks below. They visit every
        // instance in the view, so a per-item `list-sessions` fork scales with
        // the whole store, measured as the dominant tmux cost of this pass on
        // a store of a few hundred sessions.
        let live = crate::tmux::LiveSessionSnapshot::new();
        {
            let mut set_batch: Vec<(String, String, String)> = Vec::new();
            let mut unset_batch: Vec<(String, String)> = Vec::new();
            for inst in view.instances.values() {
                // This publication is one-shot: no reload re-runs it and a
                // poller does not re-emit an unchanged sid, so a row dropped
                // here stays unpublished until an unrelated sid change or a
                // relaunch. A snapshot that could not reach the server is
                // therefore probed per row rather than read as "no live pane".
                let Some(tmux_name) = inst.tmux_env_session_name_in_or_probe(&live) else {
                    continue;
                };

                set_batch.push((
                    tmux_name.clone(),
                    crate::tmux::env::AOE_INSTANCE_ID_KEY.to_string(),
                    inst.id.clone(),
                ));
                if let Some(ref sid) = inst.agent_session_id {
                    set_batch.push((
                        tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                        sid.clone(),
                    ));
                } else {
                    unset_batch.push((
                        tmux_name,
                        crate::tmux::env::AOE_CAPTURED_SESSION_ID_KEY.to_string(),
                    ));
                }
            }
            if !set_batch.is_empty() {
                let batch_refs: Vec<(&str, &str, &str)> = set_batch
                    .iter()
                    .map(|(s, k, v)| (s.as_str(), k.as_str(), v.as_str()))
                    .collect();
                if let Err(e) = crate::tmux::env::set_hidden_env_batch(&batch_refs) {
                    tracing::warn!(target: "tui.home", "Batch env sync failed: {}", e);
                }
            }
            if !unset_batch.is_empty() {
                let batch_refs: Vec<(&str, &str)> = unset_batch
                    .iter()
                    .map(|(s, k)| (s.as_str(), k.as_str()))
                    .collect();
                if let Err(e) = crate::tmux::env::remove_hidden_env_batch(&batch_refs) {
                    tracing::warn!(target: "tui.home", "Batch env unset failed: {}", e);
                }
            }
        }

        // Recover session IDs for pre-existing sessions via pollers.
        for inst in view.instances.values_mut() {
            let has_live_tmux = inst.has_live_tmux_pane_in(&live);
            if !has_live_tmux {
                continue;
            }

            inst.repair_session_id_poller_if_needed(&live);
        }

        view.refresh_registered_projects();
        view.flat_items = view.build_flat_items();
        view.update_selected();
        // Disk subscriptions stay scoped to the loaded storages: in
        // single-profile mode (`aoe --profile X`) the user opted into
        // exactly that profile's instance state, so we don't watch
        // sessions.json/groups.json for unrelated profiles. Sorted so
        // the install-loop's last-write-wins target stays stable across
        // HashMap rehash between rewire ticks (see #2584).
        let mut initial_disk_profiles: Vec<String> = view.storages.keys().cloned().collect();
        initial_disk_profiles.sort();
        view.rewire_disk_subscriptions(&initial_disk_profiles);
        // Trashed-worktree relocation (#2522) and the repair of a worktree
        // moved outside aoe (#2002) are healing work, not render input, and
        // cost a git spawn and a storage write per broken row. They sweep the
        // loaded profiles on a worker so they never delay the first frame
        // (#3611).
        view.reconcile_poller.request(initial_disk_profiles.clone());
        // Startup auto-recovery restarts resume-capable sessions whose tmux
        // pane is missing, launching each from its recorded `project_path` and
        // recording the attempt in a boot-scoped ledger that is not retried.
        // It therefore has to wait for the sweep above: a row whose worktree
        // moved outside aoe (#2002) still carries the stale path until the
        // sweep repoints it, and recovering from the stale path would burn
        // that row's only attempt for the whole boot.
        // `release_startup_recovery_gate` starts it once the sweep lands, or on
        // a deadline if it never does.
        view.startup_recovery_gate = Some(std::time::Instant::now());
        // Config subscriptions are intentionally asymmetric: even in
        // single-profile mode, peer edits to ANY profile's config.toml
        // (or the global config) must be observable so the picker UI
        // and status-hook config cache reflect external changes (e.g.
        // a peer process creating a new profile while the user runs in
        // filtered mode). The reload helper rewires the same way on
        // every tick once running, so this is the startup-side
        // counterpart that closes the boot-time window.
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

    /// Full reload: status-hook config-cache refresh + storage. Used by
    /// the 5s heartbeat tick and by event-driven sites (attach-return,
    /// save+reload pairs, profile switch). Watcher-driven ticks call
    /// `reload_storage_only` because the disk watcher only fires on
    /// `sessions.json` / `groups.json`; the config watcher drives
    /// `refresh_from_config` independently.
    pub fn reload(&mut self) -> anyhow::Result<()> {
        self.refresh_status_hook_config_cache();
        self.reload_storage_only()
    }

    /// Storage-only reload: profile rediscovery + per-profile load + tree
    /// rebuild + cursor restore. Skips the status-hook config-cache refresh,
    /// which is driven by the full `reload()` path. Used by the watcher-
    /// driven tick.
    pub(in crate::tui) fn reload_storage_only(&mut self) -> anyhow::Result<()> {
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

        // Asymmetric rewire mirrors `HomeView::new` startup wiring. Config
        // rewire covers the full `list_profiles()` set so peer config edits
        // surface to the picker UI and status-hook cache regardless of mode
        // (e.g. a peer process creating a new profile while the user runs
        // `aoe --profile X`). Disk rewire is scoped: unified mode tracks
        // every profile, single-profile mode stays bounded to
        // `self.storages.keys()` (the active profile, plus any profile
        // loaded via `move_to_profile`). Helpers are set-diff idempotent,
        // so the unconditional call is a no-op on a stable profile set.
        self.rewire_config_subscriptions(&current_profiles);
        if self.active_profile.is_some() {
            let mut active_only: Vec<String> = self.storages.keys().cloned().collect();
            active_only.sort();
            self.rewire_disk_subscriptions(&active_only);
        } else {
            self.rewire_disk_subscriptions(&current_profiles);
        }

        // Storage rebuild: unified mode only. Single-profile mode keeps the
        // explicit scope set at startup; only the active profile is loaded
        // into memory.
        if self.active_profile.is_none() {
            for name in &current_profiles {
                if !self.storages.contains_key(name) {
                    self.storages
                        .insert(name.clone(), Storage::new(name, self.file_watch.clone())?);
                }
            }
            self.storages.retain(|k, _| current_profiles.contains(k));
        }

        // Collect per-profile state without publishing it, so duplicate
        // detection (#3459) can run journal-guided repairs before anything
        // reaches the unified map.
        type ProfileLoads = Vec<(String, Vec<Instance>, Vec<Group>)>;
        let collect_loads = |storages: &HashMap<String, Storage>,
                             prev: &indexmap::IndexMap<String, Instance>|
         -> anyhow::Result<ProfileLoads> {
            let mut loads = Vec::new();
            for (profile_name, storage) in storages {
                let (mut instances, groups) = storage.load_with_groups()?;
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
        let mut loads = collect_loads(&self.storages, &self.instances)?;
        let loads_view: Vec<(&str, &[Instance])> = loads
            .iter()
            .map(|(name, instances, _)| (name.as_str(), instances.as_slice()))
            .collect();
        let storages_view: Vec<(&str, &Storage)> = self
            .storages
            .iter()
            .map(|(name, storage)| (name.as_str(), storage))
            .collect();
        let outcome = crate::session::reconcile_profile_duplicates(&loads_view, &storages_view);
        if outcome.repaired {
            // Durable state changed under lock; reload so exactly one row per
            // session is published.
            loads = collect_loads(&self.storages, &self.instances)?;
        }
        log_legacy_duplicates_once(&outcome.reports);
        self.legacy_duplicate_reports = outcome.reports;

        for (profile_name, instances, groups) in &loads {
            // Rebuild this profile's tree from disk, preserving any collapsed
            // state that was toggled in-memory but not yet on disk
            let mut new_tree = GroupTree::new_with_groups(instances, groups);
            if let Some(old_tree) = self.group_trees.get(profile_name) {
                for g in old_tree.get_all_groups() {
                    if g.collapsed {
                        new_tree.set_collapsed(&g.path, true);
                    }
                }
            }
            self.group_trees.insert(profile_name.clone(), new_tree);
            all_instances.extend(instances.iter().cloned());
        }

        // Remove trees for profiles that no longer exist
        let storage_keys: Vec<String> = self.storages.keys().cloned().collect();
        self.group_trees.retain(|k, _| storage_keys.contains(k));

        // Snapshot the in-flight Creating stub before `self.instances` is
        // overwritten. An intervening save may have persisted it, but while it
        // is still memory-only it would otherwise vanish across reload.
        let creating_stub_snapshot: Option<Instance> = self
            .creating_stub_id
            .as_ref()
            .and_then(|id| self.instances.get(id).cloned());

        self.instances = Self::build_instances_map(all_instances);

        if let Some(stub) = creating_stub_snapshot {
            self.instances.entry(stub.id.clone()).or_insert(stub);
        }

        // Refresh the project registry so project view's empty pinned headers
        // and pin indicators reflect the current on-disk registry.
        self.refresh_registered_projects();

        // Drop memoized remote-owner lookups so a `git remote add`/`set-url`
        // run since the last reload is picked up on the next org-mode
        // rebuild, instead of sticking with a stale cached owner (or a
        // stale cached "no owner") for the rest of the process. Cheap: this
        // only re-reads local `.git/config` state, no network access, and
        // this reload path already runs on a multi-second cadence, not
        // per-render.
        self.remote_owner_cache.borrow_mut().clear();

        // Remember what the cursor was pointing at so we can follow it
        let prev_selected_session = self.selected_session.clone();
        let prev_selected_group = self.selected_group.clone();

        self.rebuild_flat_items();

        // Try to restore cursor to the same session/group after rebuild
        let mut restored = false;
        if let Some(ref sid) = prev_selected_session {
            for (idx, item) in self.flat_items.iter().enumerate() {
                if let Item::Session { id, .. } = item {
                    if id == sid {
                        self.cursor = idx;
                        restored = true;
                        break;
                    }
                }
            }
        } else if let Some(ref gpath) = prev_selected_group {
            for (idx, item) in self.flat_items.iter().enumerate() {
                if let Item::Group { path, .. } = item {
                    if path == gpath {
                        self.cursor = idx;
                        restored = true;
                        break;
                    }
                }
            }
        }
        if !restored && self.cursor >= self.flat_items.len() && !self.flat_items.is_empty() {
            self.cursor = self.flat_items.len() - 1;
        }

        if self.search_active && !self.search_query.value().is_empty() {
            self.update_search();
        } else if !self.search_matches.is_empty() {
            // Recalculate match indices without moving the cursor
            self.refresh_search_matches();
        }

        self.update_selected();
        Ok(())
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
    /// * No-op: nothing failing, body unchanged, or an unrelated
    ///   dialog (a `Watcher Warning` from `rewire_after_profile_delete`,
    ///   or a profile create/delete `Error`) occupies the slot. In
    ///   the foreign-dialog case the ack latch stays armed so the
    ///   next tick can present once the foreign dialog is dismissed.
    pub(in crate::tui) fn try_present_reload_failure_dialog(&mut self) -> bool {
        if !self.reload_failure_state.has_any_failure() {
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
