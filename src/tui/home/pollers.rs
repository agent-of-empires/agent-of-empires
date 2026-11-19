//! Typed runtime completion polling; durable lifecycle work belongs to the daemon.

use super::*;
use crate::tui::dialogs::PromptOrigin;
use crate::tui::suggestions::{PrepareTarget, Start, StartAnswer, SuggestionsView};

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
        self.suggestions_feed
            .set_profile(self.active_profile.clone());
        self.suggestions_feed
            .tick(self.session_feed.latest_snapshot().is_some());
        let mut polled = self.suggestions_feed.poll();
        let mut review_closed = false;
        if self
            .confirm_dialog
            .as_ref()
            .is_some_and(|d| d.action() == "approve_suggestion_hooks")
            && !self.suggestions_feed.has_held_trust()
        {
            self.confirm_dialog = None;
            review_closed = true;
        }
        let mut changed = review_closed
            || match &mut self.suggestions_view {
                Some(view) => view.apply(&self.suggestions_feed, &mut polled),
                None => false,
            };
        if let Some(start) = self
            .suggestions_view
            .as_mut()
            .and_then(SuggestionsView::take_prepared)
        {
            self.start_suggestion(start);
            changed = true;
        }
        if polled.changed {
            self.rebuild_flat_items_keeping_cursor();
            self.update_selected();
            changed = true;
        }
        // A view prepare answered after the view closed has no one to start it.
        if let Some((key, mut start, target @ (PrepareTarget::Home | PrepareTarget::HomeAnother))) =
            polled.start
        {
            if let Some(view) = &mut self.suggestions_view {
                // The full view owns suggestion starts now, so say why nothing opened.
                view.set_error("Suggestion ready, press Start again in this list");
            } else if self.cursor_suggestion().as_ref() == Some(&key) && self.new_dialog.is_none() {
                if target == PrepareTarget::HomeAnother {
                    start.suggestion = None;
                }
                self.start_suggestion(start);
            } else {
                self.flash_status("Selection changed, suggestion not started");
            }
            changed = true;
        }
        if let Some((key, answer)) = polled.started {
            self.apply_start_answer(key, answer);
            changed = true;
        }
        if let Some(error) = polled.error {
            self.flash_status(error);
            changed = true;
        }
        changed
    }

    /// Shows what the daemon answered to a start, on the surface that asked.
    fn apply_start_answer(&mut self, key: (String, String), answer: StartAnswer) {
        let say = |this: &mut Self, message: String, error: bool| match &mut this.suggestions_view {
            Some(view) if error => view.set_error(message),
            Some(view) => view.set_notice(message),
            None => this.flash_status(message),
        };
        match answer {
            StartAnswer::Started { session_id, title } => {
                self.suggestions_feed.link_started(&key, &session_id);
                say(self, format!("Started {title}"), false);
            }
            StartAnswer::AlreadyStarted(Some(session_id)) => {
                self.suggestions_feed.link_started(&key, &session_id);
                say(self, "Already started. Enter opens it.".into(), false);
            }
            StartAnswer::AlreadyStarted(None) => {
                say(self, "Already starting. Reload in a moment.".into(), false);
            }
            StartAnswer::NeedsTrust {
                commands,
                fingerprint,
            } => {
                let mut message = String::from("This repository runs these commands:\n\n");
                for command in &commands {
                    message.push_str(&format!("  {command}\n"));
                }
                message.push_str("\nApprove them and start?");
                self.confirm_dialog = Some(
                    crate::tui::dialogs::ConfirmDialog::new(
                        "Review repository hooks",
                        &message,
                        "approve_suggestion_hooks",
                    )
                    .neutral()
                    .buttons("Approve and start", "Cancel"),
                );
                self.suggestions_feed.hold_trust(key, fingerprint);
            }
            StartAnswer::NoProject => match &mut self.suggestions_view {
                Some(view) => view.prepare(key, false, &mut self.suggestions_feed),
                None => self.suggestions_feed.prepare(key),
            },
            StartAnswer::Failed(reason) => match &mut self.suggestions_view {
                Some(view) => view.set_failed(key, reason),
                None => self.flash_status(format!("{reason} Enter tries again.")),
            },
        }
    }

    pub(super) fn open_suggestions_view(&mut self) {
        let mut view = SuggestionsView::new(&mut self.suggestions_feed);
        view.strict = self.strict_hotkeys;
        self.suggestions_view = Some(view);
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
                } else {
                    self.flash_status(message);
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
        let profile = self.config_profile();
        let profiles =
            crate::session::list_profiles_for_display().unwrap_or_else(|_| vec![profile.clone()]);
        let mut dialog = NewSessionDialog::new(
            self.available_tools.clone(),
            self.all_group_paths(),
            &profile,
            profiles,
        );
        // Path first: setting it re-resolves the defaults the rest builds on.
        let path_known = start.path.is_some();
        match start.path {
            Some(path) => dialog.set_path(path),
            None => dialog.require_path(),
        }
        dialog.set_tool(&start.tool);
        dialog.set_preset(
            start.preset.structured,
            start.preset.worktree,
            start.preset.sandbox,
            start.preset.yolo,
        );
        dialog.set_title(start.title);
        dialog.set_suggestion(
            start.initial_prompt,
            PromptOrigin {
                provider: start.provider,
                source_ref: start.source_ref,
                reason: start.reason,
                source_text: start.source_text,
                suggestion: start.suggestion,
            },
        );
        if path_known {
            dialog.focus_title();
        }
        Ok(dialog)
    }
}
