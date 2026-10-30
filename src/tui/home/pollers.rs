//! Typed runtime completion polling; durable lifecycle work belongs to the daemon.

use super::*;
use crate::tui::suggestions::Poll;

impl HomeView {
    pub(super) const CANONICAL_RELOAD_RETRY_INTERVAL: std::time::Duration =
        std::time::Duration::from_secs(5);

    pub fn apply_restart_results(&mut self) -> bool {
        let before = self.restart_in_flight.len();
        let drained = self.session_feed.drain_command_errors();
        let presented = self.present_command_errors(drained);
        self.restart_in_flight
            .retain(|id| self.session_feed.has_pending(id));
        before != self.restart_in_flight.len() || presented
    }

    pub fn request_serve_probe(&mut self) {
        if !self.serve_probe_in_flight {
            self.serve_probe_in_flight = true;
            self.serve_probe.request(());
        }
    }

    /// Applies before requesting so a queued result frees the in-flight slot
    /// for a due probe. True when the running state changed.
    pub fn tick_serve_probe(&mut self, due: bool) -> bool {
        let changed = self.apply_serve_probe();
        if due {
            self.request_serve_probe();
        }
        changed
    }

    /// True when the running state changed.
    fn apply_serve_probe(&mut self) -> bool {
        match self.serve_probe.try_recv() {
            Ok(running) => {
                self.serve_probe_in_flight = false;
                std::mem::replace(&mut self.serve_running, running) != running
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => false,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.serve_probe_in_flight = false;
                false
            }
        }
    }

    /// Drains the suggestions worker. A prepared suggestion replaces the view
    /// with the regular new-session dialog, prefilled. True when state changed.
    pub fn apply_suggestions_results(&mut self) -> bool {
        let Some(view) = &mut self.suggestions_view else {
            return false;
        };
        match view.poll() {
            Poll::Idle => false,
            Poll::Changed => true,
            Poll::Prepared(prepared) => {
                self.suggestions_view = None;
                self.open_suggestion_dialog(prepared);
                true
            }
        }
    }

    fn open_suggestion_dialog(&mut self, prepared: crate::tui::suggestions::Prepared) {
        if self.creating_stub_id.is_some() {
            self.flash_status("A session is already being created");
            return;
        }
        if !self.available_tools.any_available() {
            self.show_no_agents();
            return;
        }
        let Some(project) = prepared.project else {
            return;
        };
        let profile = self
            .profile_for_cursor(self.cursor)
            .unwrap_or_else(|| self.config_profile());
        let profiles =
            crate::session::list_profiles_for_display().unwrap_or_else(|_| vec![profile.clone()]);
        let mut dialog = NewSessionDialog::new(
            self.available_tools.clone(),
            self.all_group_paths(),
            &profile,
            profiles,
        );
        // Path first: setting it re-resolves the defaults the rest builds on.
        dialog.set_path(project.path);
        dialog.set_tool(&prepared.tool);
        dialog.set_title(prepared.title);
        dialog.set_initial_prompt(prepared.initial_prompt);
        dialog.focus_title();
        self.new_dialog = Some(dialog);
    }
}
