//! Which overlay is up, and what it takes over from the list view.

use super::*;

impl HomeView {
    /// Expire the settings view's transient "Settings saved" toast when its
    /// window passes, so it fades even while the keyboard is idle. Returns true
    /// when a redraw is needed. No-op when the settings overlay isn't open.
    pub fn tick_settings_status(&mut self) -> bool {
        self.settings_view
            .as_mut()
            .map(|view| view.tick_status())
            .unwrap_or(false)
    }

    /// Tick dialog animations/timers and drain hook progress.
    /// Returns true when a redraw is needed.
    pub fn tick_dialog(&mut self) -> bool {
        use crate::session::config::repo_config::HookProgress;

        let mut changed = false;

        if let Some(dialog) = &mut self.new_dialog {
            if dialog.tick() {
                changed = true;
            }

            if dialog.is_loading() {
                // Drain all pending hook progress messages
                while let Some(progress) = self.creation_poller.try_recv_progress() {
                    dialog.push_hook_progress(progress);
                    changed = true;
                }
            }
        }

        // Poll serve dialog for subprocess startup events.
        if let Some(view) = &mut self.serve_view {
            if view.tick() {
                changed = true;
            }
        }

        // Poll the plugin manager's in-flight discovery / update-check task.
        if let Some(dialog) = &mut self.plugin_manager_dialog {
            if dialog.tick() {
                changed = true;
            }
        }

        // Poll the skills manager's in-flight share.
        if let Some(dialog) = &mut self.skills_manager_dialog {
            if dialog.tick() {
                changed = true;
            }
        }

        // Drain hook progress into the creating buffer when no dialog is open
        if self.new_dialog.is_none() {
            if let Some(ref stub_id) = self.creating_stub_id {
                let stub_id = stub_id.clone();
                if let Some(progress_buf) = self.creating_hook_progress.get_mut(&stub_id) {
                    while let Some(progress) = self.creation_poller.try_recv_progress() {
                        match progress {
                            HookProgress::Started(cmd) => {
                                progress_buf.current_hook = Some(cmd);
                            }
                            HookProgress::Output(line) => {
                                progress_buf.hook_output.push(line);
                                // Cap buffer to prevent unbounded memory growth
                                if progress_buf.hook_output.len() > 1000 {
                                    progress_buf.hook_output.drain(..500);
                                }
                            }
                        }
                        changed = true;
                    }
                }
            }
        }

        changed
    }

    /// Whether the user is currently looking at a surface where they're
    /// likely to want to copy text (URLs, error messages, release notes).
    /// The App uses this to release xterm mouse capture so the terminal's
    /// native drag-to-select works without a modifier; mouse capture comes
    /// back as soon as the surface is dismissed.
    ///
    /// Add new dialogs here only when their content is meant to be copied,
    /// not for every modal: capture toggling has a small but visible cost
    /// (the wheel-scroll on the dashboard preview won't work while it's
    /// off).
    pub fn wants_text_selection(&self) -> bool {
        let serve_open = self.serve_view.is_some();

        serve_open
            || self.info_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self
                .intro_dialog
                .as_ref()
                .is_some_and(|d| d.wants_text_selection())
    }

    /// Same membership as `has_dialog()` minus live-send. Two callers:
    ///
    /// - List-row click routing: clicks must keep working in live mode
    ///   (that's how the user switches the live target by clicking another
    ///   row), but every other modal surface should still freeze the list.
    /// - Preview-only fast path gate (`App::draw_preview_only`): the fast
    ///   path is exactly what live-send wants, so live-send itself can't
    ///   gate it off; any OTHER overlay does, since the fast path repaints
    ///   the snapshot underneath and only re-renders the preview pane.
    ///
    /// `has_dialog()` ORs `live_send.is_some()` on top, so it would also
    /// gate off the fast path it's supposed to enable — that's why the
    /// fast path needs this method instead.
    pub(in crate::tui) fn has_non_live_send_overlay(&self) -> bool {
        let serve_open = self.serve_view.is_some();

        self.show_help
            || self.search_active
            || self.new_dialog.is_some()
            || self.confirm_dialog.is_some()
            || self.unified_delete_dialog.is_some()
            || self.group_delete_options_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.worktree_name_dialog.is_some()
            || self.restart_dialog.is_some()
            || self.context_menu.is_some()
            || self.repo_trust_dialog.is_some()
            || self.hooks_install_dialog.is_some()
            || self.volume_ignores_glob_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.info_dialog.is_some()
            || self.snooze_duration_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.attach_project_dialog.is_some()
            || self.plugin_manager_dialog.is_some()
            || self.skills_manager_dialog.is_some()
            || self.command_palette.is_some()
            || self.tool_picker_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.permission_response_dialog.is_some()
            || self.update_confirm_dialog.is_some()
            || self.telemetry_consent_dialog.is_some()
            || self.tips_dialog.is_some()
            || serve_open
            || self.settings_view.is_some()
            || self.diff_view.is_some()
    }

    /// True when live-send owns the keyboard: live mode is active and no
    /// other overlay has stolen focus on top of it. This is the same
    /// predicate `handle_key` uses to route keys straight to the agent pane,
    /// lifted into a helper so the app-level global keybindings (Ctrl+C in
    /// particular) can defer to live-send instead of quitting aoe (#2894).
    pub(in crate::tui) fn is_live_send_capturing(&self) -> bool {
        self.live_send.is_some() && !self.has_non_live_send_overlay()
    }

    /// Arm the live-send footer's "Ctrl+C sent to agent" flash. Called each
    /// time a Ctrl+C is forwarded to the agent in live mode so the reminder
    /// re-shows on every press (#2894).
    pub(in crate::tui) fn flash_ctrl_c_hint(&mut self) {
        self.live_send_ctrl_c_flash_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(3));
    }

    /// Whether the "Ctrl+C sent to agent" footer flash is currently within
    /// its display window.
    pub(in crate::tui) fn live_send_ctrl_c_flash_active(&self) -> bool {
        self.live_send_ctrl_c_flash_until
            .is_some_and(|deadline| std::time::Instant::now() < deadline)
    }

    pub fn has_dialog(&self) -> bool {
        let serve_open = self.serve_view.is_some();

        self.live_send.is_some()
            || self.show_help
            || self.search_active
            || self.new_dialog.is_some()
            || self.confirm_dialog.is_some()
            || self.unified_delete_dialog.is_some()
            || self.group_delete_options_dialog.is_some()
            || self.rename_dialog.is_some()
            || self.worktree_name_dialog.is_some()
            || self.restart_dialog.is_some()
            || self.context_menu.is_some()
            || self.repo_trust_dialog.is_some()
            || self.hooks_install_dialog.is_some()
            || self.volume_ignores_glob_dialog.is_some()
            || self.intro_dialog.is_some()
            || self.no_agents_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self.info_dialog.is_some()
            || self.snooze_duration_dialog.is_some()
            || self.profile_picker_dialog.is_some()
            || self.project_session_picker_dialog.is_some()
            || self.projects_dialog.is_some()
            || self.attach_project_dialog.is_some()
            || self.plugin_manager_dialog.is_some()
            || self.skills_manager_dialog.is_some()
            || self.command_palette.is_some()
            || self.tool_picker_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.permission_response_dialog.is_some()
            || self.update_confirm_dialog.is_some()
            || self.telemetry_consent_dialog.is_some()
            || self.tips_dialog.is_some()
            || serve_open
            || self.settings_view.is_some()
            || self.diff_view.is_some()
    }

    /// Whether the paste-burst detector should fire for incoming key events.
    ///
    /// The detector exists to solve the home-view shortcut-shadowing problem:
    /// Mosh strips bracketed-paste markers, so a pasted stream of `KeyCode::Char`
    /// events would fire `n`/`d`/`r`/etc. shortcuts on the home view. When a
    /// dialog captures keys into a text input, those shortcuts don't fire —
    /// but the dialog also won't receive a synthesized `Paste` event unless
    /// it routes through `handle_paste`. Bursting through a dialog that only
    /// handles `Key` events strands the text in `pending_paste` and leaves
    /// the dialog's input empty.
    ///
    /// So: burst is safe when no dialog is open (home shortcuts at risk) or
    /// when one of the four paste-routed dialogs is open (rename / send_message
    /// / new / settings — each forwards to `handle_paste`). For every other
    /// dialog (command palette, profile picker, projects, info, etc.) keys
    /// must dispatch individually so the dialog input receives them.
    pub fn wants_paste_burst(&self) -> bool {
        if !self.has_dialog() {
            return true;
        }
        // Live-send mode is also paste-aware: handle_paste forwards
        // the chunk straight to the pane via the control-mode worker,
        // which is strictly faster and safer than letting the chars
        // fan out as individual KeyEvents and stream per-char tmux
        // commands.
        self.live_send.is_some()
            || self.rename_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.new_dialog.is_some()
            || self.settings_view.is_some()
    }
}
