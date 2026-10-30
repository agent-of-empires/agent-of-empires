//! Typed runtime completion polling; durable lifecycle work belongs to the daemon.

use super::*;
use crate::tui::dialogs::PromptOrigin;
use crate::tui::suggestions::{Start, SuggestionsView};

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

    /// Drains the suggestions worker. True when state changed.
    pub fn apply_suggestions_results(&mut self) -> bool {
        let mut changed = self
            .suggestions_view
            .as_mut()
            .is_some_and(SuggestionsView::poll);
        self.suggestions_feed.tick();
        let polled = self.suggestions_feed.poll();
        if polled.changed {
            self.rebuild_flat_items_keeping_cursor();
            self.update_selected();
            changed = true;
        }
        if let Some(start) = polled.start {
            self.start_suggestion(start);
            changed = true;
        }
        if let Some(error) = polled.error {
            self.flash_status(error);
            changed = true;
        }
        changed
    }

    /// Replaces the view with the regular new-session dialog, prefilled; when
    /// the dialog cannot open the view stays and shows why.
    pub(super) fn start_suggestion(&mut self, start: Start) {
        match self.suggestion_dialog(start) {
            Ok(dialog) => {
                self.suggestions_view = None;
                self.new_dialog = Some(dialog);
            }
            Err(message) => {
                if let Some(view) = &mut self.suggestions_view {
                    view.set_error(message);
                }
            }
        }
    }

    fn suggestion_dialog(&self, start: Start) -> Result<NewSessionDialog, &'static str> {
        if self.creating_stub_id.is_some() {
            return Err("A session is already being created");
        }
        if !self.available_tools.any_available() {
            return Err("No agent is available. Set one up before starting a suggestion");
        }
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
        dialog.set_path(start.path);
        dialog.set_tool(&start.tool);
        dialog.set_title(start.title);
        dialog.set_suggestion(
            start.initial_prompt,
            PromptOrigin {
                provider: start.provider,
                source_ref: start.source_ref,
                reason: start.reason,
                source_text: start.source_text,
            },
        );
        dialog.focus_title();
        Ok(dialog)
    }
}
