//! Suggested-session browsing and normal wizard submission through the daemon.

use super::*;
use crate::tui::app::Action;
use crate::tui::suggestions::{
    SuggestionActionKind, SuggestionsClient, SuggestionsCommand, SuggestionsView,
};

impl HomeView {
    pub(super) fn open_suggestions(&mut self) {
        let mut view = SuggestionsView::default();
        view.refresh(self.config_profile());
        self.suggestions_view = Some(view);
    }

    pub(super) fn handle_suggestions_key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> Option<Action> {
        let profile = self.config_profile();
        let command = self.suggestions_view.as_mut()?.handle_key(key);
        match command {
            SuggestionsCommand::Close => {
                self.suggestions_view = None;
                self.pending_suggestion_customization = None;
            }
            SuggestionsCommand::OpenLinked(id) => {
                if self.get_instance(&id).is_some() {
                    self.suggestions_view = None;
                    self.select_and_reveal_session(&id);
                    return self.activate_selected_session();
                }
                self.suggestions_view.as_mut()?.error = Some("Linked session is unavailable in this local profile; refresh sessions and try again.".into());
            }
            SuggestionsCommand::ReviewPolicy => {
                let review = self
                    .suggestions_view
                    .as_mut()?
                    .review_selected_policy()
                    .unwrap_or_else(|| "Select a suggestion before reviewing automation.".into());
                self.info_dialog = Some(InfoDialog::sized_to_fit(
                    "Review automation policy",
                    &review,
                ));
            }
            command => {
                let view = self.suggestions_view.as_mut()?;
                match command {
                    SuggestionsCommand::Refresh => view.refresh(profile),
                    SuggestionsCommand::ToggleDismissed => {
                        view.show_dismissed = !view.show_dismissed;
                        view.selected = 0;
                        view.preview_scroll = 0;
                    }
                    SuggestionsCommand::TogglePolicy => view.toggle_selected_policy(profile),
                    SuggestionsCommand::Blocked(reason) => view.error = Some(reason),
                    SuggestionsCommand::Start(id) => {
                        view.action(profile, id, SuggestionActionKind::Start, false)
                    }
                    SuggestionsCommand::CreateAnother(id) => {
                        view.action(profile, id, SuggestionActionKind::Start, true)
                    }
                    SuggestionsCommand::Dismiss(id) => {
                        view.action(profile, id, SuggestionActionKind::Dismiss, false)
                    }
                    SuggestionsCommand::Restore(id) => {
                        view.action(profile, id, SuggestionActionKind::Restore, false)
                    }
                    SuggestionsCommand::Customize(id) => view.prepare(profile, id),
                    _ => {}
                }
            }
        }
        None
    }

    pub(super) fn tick_suggestions(&mut self) -> bool {
        let Some(view) = &mut self.suggestions_view else {
            return false;
        };
        let was_busy = view.is_busy();
        view.poll_refresh();
        let changed = was_busy != view.is_busy();
        let prepared = view.prepared.take();
        if let Some((prepared, _)) = prepared {
            let title = prepared.title.clone();
            let path = prepared.project_path.clone();
            self.open_new_session_dialog();
            if let Some(dialog) = &mut self.new_dialog {
                dialog.set_title(title);
                dialog.set_path(path);
                self.pending_suggestion_customization = Some((self.config_profile(), prepared));
            }
            return true;
        }
        changed
    }

    pub(super) fn request_customized_suggestion(&mut self, data: &NewSessionData) {
        if self.pending_customized_session_create.is_some() {
            return;
        }
        let Some((profile, prepared)) = &self.pending_suggestion_customization else {
            return;
        };
        let body = if &data.profile != profile {
            Err(anyhow::anyhow!("Keep the suggestion's profile when customizing; cancel and reopen Suggestions in another profile to switch."))
        } else {
            SuggestionsClient::create_customized_session_body(data, prepared)
        };
        let body = match body {
            Ok(body) => body,
            Err(error) => {
                if let Some(dialog) = &mut self.new_dialog {
                    dialog.set_error(error.to_string());
                }
                return;
            }
        };
        let profile = profile.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.pending_customized_session_create = Some(rx);
        if let Some(dialog) = &mut self.new_dialog {
            dialog.set_loading(true);
        }
        std::thread::spawn(move || {
            let result = tokio::runtime::Builder::new_current_thread().enable_all().build()
                .map_err(|e| e.to_string()).and_then(|runtime| runtime.block_on(async move {
                    let endpoint = crate::acp::client::require_daemon().await.map_err(|e| e.to_string())?;
                    let client = SuggestionsClient::new(endpoint, profile).map_err(|e| e.to_string())?;
                    let result = client.create_session(body).await.map_err(|e| e.to_string())?;
                    result.get("id").or_else(|| result.get("session_id"))
                        .and_then(serde_json::Value::as_str).map(str::to_owned)
                        .ok_or_else(|| "Session creation returned no session id; refresh sessions before retrying.".into())
                }));
            let _ = tx.send(result);
        });
    }

    pub(crate) fn apply_suggestion_creation(&mut self) -> Option<Result<String, String>> {
        let result = match self
            .pending_customized_session_create
            .as_ref()
            .map(|rx| rx.try_recv())
        {
            Some(Ok(result)) => Some(result),
            Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => Some(Err(
                "Suggested session creation ended without a result.".into(),
            )),
            _ => None,
        };
        if let Some(result) = result {
            self.pending_customized_session_create = None;
            if let Some(dialog) = &mut self.new_dialog {
                dialog.set_loading(false);
            }
            match result {
                Ok(id) => {
                    self.new_dialog = None;
                    self.pending_suggestion_customization = None;
                    self.suggestions_view = None;
                    return Some(Ok(id));
                }
                Err(error) => {
                    if let Some(dialog) = &mut self.new_dialog {
                        dialog.set_error(error.clone());
                    }
                    if let Some(view) = &mut self.suggestions_view {
                        view.error = Some(error.clone());
                    }
                    return Some(Err(error));
                }
            }
        }
        let id = self
            .suggestions_view
            .as_mut()
            .and_then(|view| view.completed_session_id.take())?;
        self.suggestions_view = None;
        Some(Ok(id))
    }
}
