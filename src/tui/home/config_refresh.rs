//! Reloading config into the view, whether the user asked or the watcher
//! fired.

use super::*;

impl HomeView {
    /// The profile whose config the view should resolve. The active profile
    /// when one is selected, otherwise (all-profiles mode) the user's default
    /// profile. Never an empty string and never a hard-coded name.
    pub(in crate::tui) fn config_profile(&self) -> String {
        self.active_profile
            .clone()
            .unwrap_or_else(crate::session::config::resolve_default_profile)
    }

    /// Resolve `live_send_on_view_switch` for an existing session row:
    /// whether switching into Terminal or Tool view should auto-start
    /// live-send instead of waiting for a separate Enter/Tab/click. See
    /// `resolve_session_config_for` for resolution rules.
    pub(in crate::tui) fn live_send_on_view_switch(&self, session_id: &str) -> bool {
        self.resolve_session_config_for(session_id)
            .is_some_and(|s| s.live_send_on_view_switch)
    }

    /// True when Enter on the *currently selected session row* would
    /// enter live-send mode (and Tab would swap to a tmux attach).
    /// Returns `None` when the cursor is not on a session row (group or
    /// nothing selected) so the help overlay can fall back to a stable
    /// default rather than mislabel keys that don't apply. Honors per-
    /// profile overrides via `default_attach_mode(id)`.
    pub(in crate::tui) fn help_live_on_enter(&self) -> Option<bool> {
        let id = self.selected_session.as_deref()?;
        let mode = self.default_attach_mode(id)?;
        Some(matches!(mode, crate::session::AttachMode::LiveSend))
    }

    /// Refresh config-derived state for the active profile (Interactive
    /// path). Uses the lenient `resolve_config_or_warn` so transient
    /// parse errors fall back to defaults; user-initiated callers
    /// tolerate that because the next save will fix it. The watcher
    /// path uses `try_refresh_from_config_watcher` instead, which
    /// preserves previous in-memory state on parse failure rather than
    /// silently applying defaults.
    pub(in crate::tui) fn refresh_from_config(&mut self, origin: ConfigRefreshOrigin) {
        let profile = self.config_profile();
        let config = resolve_config_or_warn(&profile);
        // The lenient resolver returns defaults when only the profile is malformed.
        let sidebar_position = crate::session::Config::load_or_warn()
            .session
            .sidebar_position;
        self.apply_config_to_state(config, sidebar_position, origin);
    }

    /// Watcher-path counterpart of `refresh_from_config`. Returns Err on
    /// TOML parse failure for the active profile so the tick loop can
    /// preserve the previous in-memory active config rather than silently
    /// flipping safety-affecting settings (e.g. `confirm_before_quit`)
    /// to defaults. The Err is consumed by `handle_tick_reload_config` in
    /// `App::run` and surfaced in the aggregated reload-failure dialog.
    ///
    /// Error attribution: `resolve_config` loads the global
    /// `<app_dir>/config.toml` first then merges per-profile overrides,
    /// so a Reload Failed dialog body rendered from this path can name
    /// a global parse error even when the watcher fired for a
    /// per-profile edit. `toml::de::Error` renders line and column
    /// without the source file path.
    pub(in crate::tui) fn try_refresh_from_config_watcher(&mut self) -> anyhow::Result<()> {
        let new_count = self
            .watcher_config_refresh_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.maybe_export_watcher_refresh_count(new_count);
        let profile = self.config_profile();
        let config = crate::session::resolve_config(&profile)?;
        let sidebar_position = config.session.sidebar_position;
        self.apply_config_to_state(config, sidebar_position, ConfigRefreshOrigin::Watcher);
        Ok(())
    }

    /// Apply a snapshot, reporting interactive warnings without interrupting watcher reloads.
    fn apply_config_to_state(
        &mut self,
        config: crate::session::Config,
        sidebar_position: crate::session::config::SidebarPosition,
        origin: ConfigRefreshOrigin,
    ) {
        self.default_terminal_mode = match config.sandbox.default_terminal_mode {
            DefaultTerminalMode::Host => TerminalMode::Host,
            DefaultTerminalMode::Container => TerminalMode::Container,
        };
        self.sound_config = config.sound.clone();
        self.strict_hotkeys = config.session.strict_hotkeys;
        self.confirm_before_quit = config.session.confirm_before_quit;
        self.host_tab_title = config.session.host_tab_title;
        self.row_tag_mode = config.session.row_tag;
        self.set_sidebar_position(sidebar_position);
        self.show_diagnostics = config.session.show_diagnostics_pane;
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
        // Watcher path: stash for tick-loop dispatch (App owns theme state).
        // Reads via `resolve_theme_name` (global-only by contract), not
        // `config.theme.name` which would carry a stale per-profile override.
        // Guard is load-bearing: Interactive already returns
        // `Action::SetTheme` directly from input handlers, so stashing
        // unconditionally would double-dispatch on every settings save.
        // Note: `resolve_theme_name` swallows read errors via `load_or_warn`
        // and falls back to "zinc"; a peer write landing between the
        // `resolve_config` above and this call momentarily flips the theme
        // and the next watcher event recovers. Diverges from the
        // "preserve prior state on Err" contract honored by other fields
        // here; acceptable since `set_theme` is idempotent and the race
        // window is microseconds wide.
        if matches!(origin, ConfigRefreshOrigin::Watcher) {
            self.pending_watcher_theme = Some(crate::session::config::resolve_theme_name());
        }
    }

    /// Drain the theme name stashed by `apply_config_to_state` on the
    /// Watcher path. The tick loop in `App::run` calls this after
    /// `try_refresh_from_config_watcher` and dispatches the result to
    /// `App::set_theme`. Returns `None` outside the watcher path or
    /// after a previous take in the same tick.
    pub(in crate::tui) fn take_pending_watcher_theme(&mut self) -> Option<String> {
        self.pending_watcher_theme.take()
    }

    /// Export the watcher-config-refresh counter to a hidden file in
    /// the app dir when `AOE_E2E_DEBUG=1` is set on the TUI process.
    /// The file (`<app_dir>/.aoe_e2e_refresh_count`) is polled by the
    /// e2e harness as a deterministic completion signal for the
    /// watcher path. Production builds and non-e2e test runs never
    /// set the env var, so the file is never written. Write failures
    /// fall through to a `tracing::trace!`; the file is debug-only,
    /// so a missing write surfaces as a harness poll timeout rather
    /// than a hard error on the TUI side.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::{profile_config::save_profile_config, SidebarPosition};
    use serial_test::serial;

    /// Invalid profile files must not replace a valid global sidebar preference.
    #[test]
    #[serial]
    fn sidebar_position_ignores_profile_overrides_on_startup_and_reload() {
        let temp = tempfile::TempDir::new().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let save_positions = |global_position, profile_position| {
            update_config(|config| config.session.sidebar_position = global_position).unwrap();
            let profile = serde_json::from_value(serde_json::json!({
                "session": { "sidebar_position": profile_position }
            }))
            .unwrap();
            save_profile_config("test", &profile).unwrap();
        };
        save_positions(SidebarPosition::Right, SidebarPosition::Left);
        let mut view = HomeView::new_for_test(
            Some("test".to_string()),
            AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        assert_eq!(view.sidebar_position, SidebarPosition::Right);

        save_positions(SidebarPosition::Left, SidebarPosition::Right);
        view.refresh_from_config(ConfigRefreshOrigin::Interactive);
        assert_eq!(view.sidebar_position, SidebarPosition::Left);

        save_positions(SidebarPosition::Right, SidebarPosition::Left);
        view.try_refresh_from_config_watcher().unwrap();
        assert_eq!(view.sidebar_position, SidebarPosition::Right);

        let profile_path = crate::session::get_profile_dir_path("test")
            .unwrap()
            .join("config.toml");
        std::fs::write(&profile_path, "[session\n").unwrap();
        view.refresh_from_config(ConfigRefreshOrigin::Interactive);
        assert_eq!(view.sidebar_position, SidebarPosition::Right);
        let reopened = HomeView::new_for_test(
            Some("test".to_string()),
            AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        assert_eq!(reopened.sidebar_position, SidebarPosition::Right);

        save_positions(SidebarPosition::Right, SidebarPosition::Left);
        std::fs::write(crate::session::config::config_path().unwrap(), "[session\n").unwrap();
        assert!(view.try_refresh_from_config_watcher().is_err());
        assert_eq!(view.sidebar_position, SidebarPosition::Right);
    }
}
