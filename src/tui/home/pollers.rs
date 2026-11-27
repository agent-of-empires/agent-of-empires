//! Typed runtime completion polling; durable lifecycle work belongs to the daemon.

use super::*;
use crate::tui::dialogs::PromptOrigin;
use crate::tui::suggestions::{Start, StartAnswer, SuggestionsView};

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
        if self.suggested_sessions_tip_armed {
            self.queue_suggested_sessions_tip();
        }
        if self
            .start_after_trash
            .as_ref()
            .is_some_and(|retry| self.session_feed.applied_epoch() != Some(retry.epoch.as_str()))
        {
            self.start_after_trash = None;
            if let Some(view) = &mut self.suggestions_view {
                view.set_notice(
                    "The runtime changed before the Trash finished, so nothing started.",
                );
            }
        }
        let blocked: std::collections::HashMap<_, _> = self
            .session_feed
            .applied_snapshot()
            .map(|snapshot| {
                snapshot
                    .contents
                    .sessions
                    .iter()
                    .filter_map(|row| {
                        let message = row.initial_prompt_error.clone()?;
                        Some((
                            row.id.clone(),
                            (message, row.initial_prompt_error_code.clone()),
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let blocked_changed = blocked != self.suggestions_feed.blocked_prompts;
        self.suggestions_feed.blocked_prompts = blocked;
        let mut review_closed = false;
        let reviewing = self
            .confirm_dialog
            .as_ref()
            .is_some_and(|d| d.action() == "approve_suggestion_hooks");
        if reviewing && !self.suggestions_feed.has_held_trust() {
            self.confirm_dialog = None;
            review_closed = true;
        }
        // The review was cancelled: nothing starts.
        if !reviewing && self.suggestions_feed.has_held_trust() {
            self.suggestions_feed.drop_held_trust();
            if let Some(view) = &mut self.suggestions_view {
                view.set_notice("Not started: the hooks were not approved.");
            }
            review_closed = true;
        }
        let applied = match &mut self.suggestions_view {
            Some(view) => {
                view.retry_pending = self.start_after_trash.is_some();
                view.apply(&mut self.suggestions_feed, &mut polled)
            }
            None => false,
        };
        let mut changed = applied || review_closed || blocked_changed;
        if let Some(start) = self
            .suggestions_view
            .as_mut()
            .and_then(SuggestionsView::take_prepared)
        {
            // A dialog only opens over the pane that asked for it.
            let note = "Not opened: you left the list. Press the key again.";
            let shown = self.suggestions_pane_shown();
            if self.suggestions_focused && !self.has_non_live_send_overlay() {
                self.start_suggestion(start);
            } else if let Some(view) = self.suggestions_view.as_mut().filter(|_| shown) {
                view.set_notice(note);
            } else {
                self.flash_status(note);
            }
            changed = true;
        }
        if polled.changed {
            self.rebuild_flat_items_keeping_cursor();
            self.update_selected();
            changed = true;
        }
        for (key, answer) in polled.started {
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
        let shown = self.suggestions_pane_shown();
        let say = |this: &mut Self, message: String, error: bool| match &mut this.suggestions_view {
            Some(view) if shown && error => view.set_error(message),
            Some(view) if shown => view.set_notice(message),
            _ => this.flash_status(message),
        };
        // Enter opens a started item only in the pane.
        let opens = if shown { " · Enter opens it" } else { "" };
        if let Some(step) = self.suggestions_feed.batch_answer(&key, &answer) {
            if let StartAnswer::Started { session_id, .. }
            | StartAnswer::AlreadyStarted(Some(session_id)) = &answer
            {
                self.suggestions_feed.link_started(&key, session_id);
                self.rebuild_flat_items_keeping_cursor();
                self.update_selected();
            }
            if let Some(note) = step.failure {
                self.suggestions_feed.failed = Some((key, note));
            }
            if let Some(summary) = step.summary {
                say(self, summary, false);
            }
            return;
        }
        match answer {
            StartAnswer::Started { session_id, title } => {
                self.suggestions_feed.link_started(&key, &session_id);
                self.rebuild_flat_items_keeping_cursor();
                self.update_selected();
                say(self, format!("Started {title}{opens}"), false);
            }
            StartAnswer::AlreadyStarted(Some(session_id)) => {
                self.suggestions_feed.link_started(&key, &session_id);
                self.rebuild_flat_items_keeping_cursor();
                self.update_selected();
                say(self, format!("Already started{opens}"), false);
            }
            StartAnswer::AlreadyStarted(None) => {
                let message = if shown {
                    let reload = if self.strict_hotkeys { "R" } else { "r" };
                    format!("Already starting. Press {reload} in a moment to reload.")
                } else {
                    "Already starting. It shows under its Project in a moment.".to_string()
                };
                say(self, message, false);
            }
            StartAnswer::TrustChanged => self.suggestions_feed.restart_after_trust_change(key),
            StartAnswer::NeedsTrust {
                commands,
                mcp_summaries,
                fingerprint,
            } => {
                // Never replace a dialog the user has not seen, such as "Prompt not delivered".
                if self.has_non_live_send_overlay() {
                    let message = "The repository hooks need review. Close the open dialog, then press Enter again.";
                    say(self, message.to_string(), true);
                    return;
                }
                let changed = self.suggestions_feed.take_hooks_changed();
                let mut message = String::from(if changed {
                    "The hooks changed since you reviewed them. This repository runs:\n\n"
                } else {
                    "This repository runs these commands:\n\n"
                });
                for command in &commands {
                    message.push_str(&format!("  {command}\n"));
                }
                for summary in &mcp_summaries {
                    message.push_str(&format!("  MCP server: {summary}\n"));
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
            StartAnswer::NoProject => {
                if let Some(view) = &mut self.suggestions_view {
                    view.prepare(key, false, &mut self.suggestions_feed);
                }
            }
            StartAnswer::Failed(reason) => match &mut self.suggestions_view {
                Some(view) if shown => view.set_failed(&mut self.suggestions_feed, key, reason),
                _ => {
                    self.flash_status(format!("Couldn't start: {reason}"));
                    self.suggestions_feed.failed = Some((key, reason));
                }
            },
        }
    }

    /// Opens the regular new-session dialog, prefilled, over Home; when the
    /// dialog cannot open the list shows why.
    pub(super) fn start_suggestion(&mut self, start: Start) {
        match self.suggestion_dialog(start) {
            Ok(dialog) => {
                self.suggestions_focused = false;
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
        dialog.set_extra_repos(start.extra_paths);
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
