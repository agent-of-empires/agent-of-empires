//! Reloading config into the view, whether the user asked or the watcher fired.

use super::*;

impl HomeView {
    pub(in crate::tui) fn config_profile(&self) -> String {
        self.active_profile
            .clone()
            .unwrap_or_else(crate::session::config::resolve_default_profile)
    }

    pub(in crate::tui) fn live_send_on_view_switch(&self, session_id: &str) -> bool {
        self.resolve_session_config_for(session_id)
            .is_some_and(|s| s.live_send_on_view_switch)
    }

    pub(in crate::tui) fn help_live_on_enter(&self) -> Option<bool> {
        let id = self.selected_session.as_deref()?;
        let mode = self.default_attach_mode(id)?;
        Some(matches!(mode, crate::session::AttachMode::LiveSend))
    }

    pub(in crate::tui) fn refresh_from_config(&mut self, origin: ConfigRefreshOrigin) {
        let profile = self.config_profile();
        let config = resolve_config_or_warn(&profile);
        self.apply_config_to_state(config, origin);
    }

    /// Strict counterpart of `refresh_from_config`: a parse error keeps the previous state
    /// instead of silently applying defaults.
    pub(in crate::tui) fn try_refresh_from_config_watcher(&mut self) -> anyhow::Result<()> {
        let new_count = self
            .watcher_config_refresh_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.maybe_export_watcher_refresh_count(new_count);
        let profile = self.config_profile();
        let config = crate::session::resolve_config(&profile)?;
        self.apply_config_to_state(config, ConfigRefreshOrigin::Watcher);
        Ok(())
    }

    fn apply_config_to_state(
        &mut self,
        config: crate::session::Config,
        origin: ConfigRefreshOrigin,
    ) {
        self.default_terminal_mode = match config.sandbox.default_terminal_mode {
            DefaultTerminalMode::Host => TerminalMode::Host,
            DefaultTerminalMode::Container => TerminalMode::Container,
        };
        self.sound_config = config.sound.clone();
        self.status_hook_config = config.status_hooks.clone();
        self.refresh_status_hook_config_cache();
        self.strict_hotkeys = config.session.strict_hotkeys;
        self.confirm_before_quit = config.session.confirm_before_quit;
        self.host_tab_title = config.session.host_tab_title;
        self.row_tag_mode = config.session.row_tag;
        self.show_diagnostics = config.session.show_diagnostics_pane;
        self.daemon_sidebar = config.session.daemon_sidebar;
        if !self.daemon_sidebar {
            self.set_sidebar_source(
                crate::tui::session_feed::SidebarSource::Storage,
                Some("session.daemon_sidebar is off"),
            );
        }
        self.agent_clipboard_forward =
            config.tmux.clipboard != crate::session::config::TmuxSettingMode::Disabled;
        self.vt_live_enabled = config.tmux.vt_live;
        if let Some(worker) = self.preview_capture_worker.as_ref() {
            worker.set_vt_enabled(
                self.vt_live_enabled && !matches!(self.view_mode, ViewMode::Terminal),
            );
            worker.set_clipboard_capture_enabled(self.agent_clipboard_forward);
        }
        self.profile_default_attach_mode = config.session.default_attach_mode;
        self.idle_decay_window =
            crate::tui::styles::idle_decay_window(config.theme.idle_decay_minutes);
        crate::session::set_unread_enabled(config.session.unread_indicator);
        crate::session::set_favorites_first(config.session.favorites_first);
        self.tips_unseen = tips_unseen_count(&config);
        self.tool_configs = config.tools;
        self.tool_hotkey_cache = input::build_tool_hotkey_cache(&self.tool_configs);
        let hotkey_warnings = input::validate_tool_hotkeys(&self.tool_configs);
        if matches!(origin, ConfigRefreshOrigin::Interactive)
            && !hotkey_warnings.is_empty()
            && self.info_dialog.is_none()
        {
            self.info_dialog = Some(InfoDialog::new(
                "Tool hotkey config errors",
                &hotkey_warnings.join("\n"),
            ));
        }
        // Interactive saves already dispatch `Action::SetTheme`; the theme is global-only.
        if matches!(origin, ConfigRefreshOrigin::Watcher) {
            self.pending_watcher_theme = Some(crate::session::config::resolve_theme_name());
        }
    }

    pub(in crate::tui) fn take_pending_watcher_theme(&mut self) -> Option<String> {
        self.pending_watcher_theme.take()
    }

    /// E2E completion signal: `<app_dir>/.aoe_e2e_refresh_count`, written only under `AOE_E2E_DEBUG=1`.
    fn maybe_export_watcher_refresh_count(&self, count: u64) {
        if std::env::var("AOE_E2E_DEBUG").as_deref() != Ok("1") {
            return;
        }
        let app_dir = match crate::session::get_app_dir() {
            Ok(p) => p,
            Err(e) => {
                tracing::trace!(
                    target: "tui.e2e_debug",
                    error = %e,
                    "AOE_E2E_DEBUG export skipped; app dir resolution failed"
                );
                return;
            }
        };
        let path = app_dir.join(".aoe_e2e_refresh_count");
        if let Err(e) = std::fs::write(&path, count.to_string()) {
            tracing::trace!(
                target: "tui.e2e_debug",
                error = %e,
                path = %path.display(),
                "AOE_E2E_DEBUG export failed"
            );
        }
    }

    pub(super) fn status_hook_profile_names(
        active_profile: Option<&str>,
        storages: &HashMap<String, Storage>,
    ) -> Vec<String> {
        let mut profile_names = match active_profile {
            Some(profile) => vec![profile.to_string()],
            None => storages.keys().cloned().collect(),
        };
        let default_profile = crate::session::config::resolve_default_profile();
        if !profile_names.contains(&default_profile) {
            profile_names.push(default_profile);
        }
        profile_names.sort();
        profile_names.dedup();
        profile_names
    }

    pub(super) fn load_status_hook_configs(
        profile_names: Vec<String>,
    ) -> HashMap<String, crate::status_hooks::StatusHookConfig> {
        profile_names
            .into_iter()
            .map(|profile| {
                let status_hooks = resolve_config_or_warn(&profile).status_hooks;
                (profile, status_hooks)
            })
            .collect()
    }

    pub(super) fn refresh_status_hook_config_cache(&mut self) {
        let profile_names =
            Self::status_hook_profile_names(self.active_profile.as_deref(), &self.storages);
        self.status_hook_configs = Self::load_status_hook_configs(profile_names);
        let profile = self.config_profile();
        if let Some(status_hooks) = self.status_hook_configs.get(&profile) {
            self.status_hook_config = status_hooks.clone();
        }
    }
}
