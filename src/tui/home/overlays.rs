//! Which overlay is up, and what it takes over from the list view.

use std::time::{Duration, Instant};

use super::*;

const FLASH_WINDOW: Duration = Duration::from_secs(3);

impl HomeView {
    /// Expire the settings view's "Settings saved" toast; true when a redraw is needed.
    pub fn tick_settings_status(&mut self) -> bool {
        self.settings_view
            .as_mut()
            .is_some_and(|view| view.tick_status())
    }

    /// Tick dialog animations/timers and drain hook progress.
    /// Returns true when a redraw is needed.
    pub fn tick_dialog(&mut self) -> bool {
        use crate::session::config::repo_config::HookProgress;

        let mut changed = false;
        if let Some(dialog) = &mut self.new_dialog {
            changed |= dialog.tick();
            if dialog.is_loading() {
                while let Some(progress) = self.creation_poller.try_recv_progress() {
                    dialog.push_hook_progress(progress);
                    changed = true;
                }
            }
        }
        if let Some(view) = &mut self.serve_view {
            changed |= view.tick();
        }
        if let Some(dialog) = &mut self.plugin_manager_dialog {
            changed |= dialog.tick();
        }
        if let Some(dialog) = &mut self.skills_manager_dialog {
            changed |= dialog.tick();
        }

        // With no dialog open, hook progress feeds the creating row's buffer.
        let progress_buf = match (&self.new_dialog, &self.creating_stub_id) {
            (None, Some(stub_id)) => self.creating_hook_progress.get_mut(stub_id),
            _ => None,
        };
        if let Some(progress_buf) = progress_buf {
            while let Some(progress) = self.creation_poller.try_recv_progress() {
                match progress {
                    HookProgress::Started(cmd) => progress_buf.current_hook = Some(cmd),
                    HookProgress::Output(line) => {
                        progress_buf.hook_output.push(line);
                        if progress_buf.hook_output.len() > 1000 {
                            progress_buf.hook_output.drain(..500);
                        }
                    }
                }
                changed = true;
            }
        }
        changed
    }

    /// Whether the visible surface holds text worth copying. The App releases
    /// mouse capture then so native drag-to-select works; only add dialogs
    /// meant to be copied from, since it also disables wheel scroll.
    pub fn wants_text_selection(&self) -> bool {
        self.serve_view.is_some()
            || self.info_dialog.is_some()
            || self.changelog_dialog.is_some()
            || self
                .intro_dialog
                .as_ref()
                .is_some_and(|d| d.wants_text_selection())
    }

    /// `has_dialog()` minus live-send. List clicks must keep working in live
    /// mode (to switch the live target), and the preview-only fast path is
    /// exactly what live-send wants, so both gate on this instead.
    pub(in crate::tui) fn has_non_live_send_overlay(&self) -> bool {
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
            || self.serve_view.is_some()
            || self.settings_view.is_some()
            || self.diff_view.is_some()
    }

    /// True when live-send owns the keyboard, so app-level bindings like
    /// Ctrl+C defer to it instead of quitting.
    pub(in crate::tui) fn is_live_send_capturing(&self) -> bool {
        self.live_send.is_some() && !self.has_non_live_send_overlay()
    }

    pub(in crate::tui) fn flash_ctrl_c_hint(&mut self) {
        self.live_send_ctrl_c_flash_until = Some(Instant::now() + FLASH_WINDOW);
    }

    pub(in crate::tui) fn live_send_ctrl_c_flash_active(&self) -> bool {
        self.live_send_ctrl_c_flash_until
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Show `text` in the status bar for a few seconds.
    pub(in crate::tui) fn flash_status(&mut self, text: impl Into<String>) {
        self.status_flash = Some((text.into(), Instant::now() + FLASH_WINDOW));
    }

    pub(in crate::tui) fn status_flash_text(&self) -> Option<&str> {
        self.status_flash
            .as_ref()
            .filter(|(_, deadline)| Instant::now() < *deadline)
            .map(|(text, _)| text.as_str())
    }

    /// Drop an expired flash; true when the caller must repaint to clear it.
    pub(in crate::tui) fn expire_status_flash(&mut self) -> bool {
        let expired = self
            .status_flash
            .as_ref()
            .is_some_and(|(_, deadline)| Instant::now() >= *deadline);
        if expired {
            self.status_flash = None;
        }
        expired
    }

    pub fn has_dialog(&self) -> bool {
        self.live_send.is_some() || self.has_non_live_send_overlay()
    }

    /// Whether the paste-burst detector should fire. Mosh strips bracketed
    /// paste, so a burst guards home shortcuts; but a dialog that only
    /// handles key events would never receive the synthesized paste, so
    /// bursts are limited to no dialog or the paste-routed surfaces.
    pub fn wants_paste_burst(&self) -> bool {
        !self.has_dialog()
            || self.live_send.is_some()
            || self.rename_dialog.is_some()
            || self.send_message_dialog.is_some()
            || self.new_dialog.is_some()
            || self.settings_view.is_some()
    }
}
