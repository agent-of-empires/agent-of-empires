//! Sending a message or a permission response to the selected session.

use super::*;

/// Map a decision to its agent-defined keystroke sequence. Pure and
/// tmux-free so the choice-to-field mapping is unit-testable without a
/// real pane; `execute_permission_response` is the only caller.
pub(super) fn permission_response_tokens(
    response: &crate::agents::PermissionResponse,
    choice: crate::tui::dialogs::PermissionResponseChoice,
) -> Option<&'static [crate::agents::KeyToken]> {
    use crate::tui::dialogs::PermissionResponseChoice::*;
    match choice {
        Allow => Some(response.allow),
        AllowAlways => response.allow_always,
        Deny => Some(response.deny),
    }
}

impl HomeView {
    pub fn set_instance_status(&mut self, id: &str, status: crate::session::Status) {
        let old_status = self.get_instance(id).map(|inst| inst.status);
        self.mutate_instance(id, |inst| inst.status = status);
        if let Some(old) = old_status {
            if old != status {
                if let Some(inst) = self.get_instance(id).cloned() {
                    self.handle_status_transition(&inst, old, status, false, true);
                }
            }
        }
    }

    /// Stamp `last_accessed_at` on a session (user-initiated interaction).
    ///
    /// Sunk rows (archived or snoozed) take the heavier `apply_user_action`
    /// path so the auto-unarchive/unsnooze side effect in `touch_last_accessed`
    /// is persisted (merge_from_tui doesn't carry those fields; without this,
    /// reload would resurrect the sink from disk) and the row leaves the
    /// Archived section visually on the same frame. Non-sunk rows stay on
    /// the cheap mutate_instance path; their only mutation is the timestamp,
    /// which save() already mirrors via merge_from_tui.
    pub fn stamp_last_accessed(&mut self, id: &str) {
        let was_sunk = self
            .instances
            .get(id)
            .map(|i| i.is_archived() || i.snoozed_until.is_some())
            .unwrap_or(false);
        if was_sunk {
            if let Err(e) = self.apply_user_action(id, |inst| inst.touch_last_accessed()) {
                tracing::warn!(
                    target: "tui.home",
                    session_id = %id,
                    error = %e,
                    "stamp_last_accessed: failed to persist auto-unsink"
                );
            }
            self.rebuild_flat_items();
        } else {
            self.mutate_instance(id, |inst| inst.touch_last_accessed());
        }
    }

    /// Run the send-message work after the dialog has been dismissed: call
    /// `ensure_pane_ready` (which may auto-start or respawn), then deliver
    /// the keystrokes. Errors are surfaced via `info_dialog` so the caller
    /// (`execute_action`) only has to clear its transient status.
    ///
    pub fn execute_send_message(&mut self, session_id: &str, message: &str) {
        let target = std::mem::replace(
            &mut self.pending_send_target,
            live_send::LiveSendTarget::Agent,
        );
        // Same pane-readiness cascades as live-send: agent runs the
        // full `ensure_pane_ready` (Docker, splash, resume); terminals
        // just need their tmux session to exist with a live pane. Every cold
        // target starts at the visible preview size, avoiding an immediate
        // full-terminal-to-preview resize and its SIGWINCH repaint.
        let boot_size = self.live_send_boot_size();
        match &target {
            live_send::LiveSendTarget::Agent => {
                let outcome = self.try_mutate_instance_writeback_on_err(session_id, |inst| {
                    inst.ensure_pane_ready_with_size(boot_size)
                        .map_err(Into::into)
                });
                match outcome {
                    Ok(Some(EnsureReadyOutcome::ResumeFailed { sid })) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Resume failed for sid {sid}; preserved for explicit retry"),
                        ));
                        return;
                    }
                    Ok(_) => {}
                    Err(err) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Cannot prepare session: {}", err),
                        ));
                        return;
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => {
                if let Err(e) = self.ensure_terminal_pane_ready(session_id, boot_size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare terminal: {}", e),
                    ));
                    return;
                }
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                if let Err(e) = self.ensure_container_terminal_pane_ready(session_id, boot_size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare container terminal: {}", e),
                    ));
                    return;
                }
            }
            live_send::LiveSendTarget::Tool(name) => {
                let name = name.clone();
                if let Err(e) = self.ensure_tool_pane_ready(session_id, &name, boot_size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Send Failed",
                        &format!("Cannot prepare tool '{}': {}", name, e),
                    ));
                    return;
                }
            }
        };
        let Some(inst) = self.get_instance(session_id) else {
            self.info_dialog = Some(InfoDialog::new(
                "Send Failed",
                "Session disappeared before the message could be sent.",
            ));
            return;
        };
        let tmux_session = match &target {
            live_send::LiveSendTarget::Agent => {
                match crate::tmux::Session::new(&inst.id, &inst.title) {
                    Ok(s) => s,
                    Err(e) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Send Failed",
                            &format!("Failed to resolve session: {}", e),
                        ));
                        return;
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => crate::tmux::Session::from_name(
                &crate::tmux::TerminalSession::resolve_name(&inst.id, &inst.title),
            ),
            live_send::LiveSendTarget::ContainerTerminal => crate::tmux::Session::from_name(
                &crate::tmux::ContainerTerminalSession::resolve_name(&inst.id, &inst.title),
            ),
            live_send::LiveSendTarget::Tool(name) => crate::tmux::Session::from_name(
                crate::tmux::ToolSession::new(&inst.id, &inst.title, name).session_name(),
            ),
        };
        // Agent gets a tool-specific Enter delay so paste-burst-aware
        // agents (e.g. Codex) don't swallow the final Enter. Shells in
        // the paired terminal panes don't need the delay.
        let delay = match &target {
            live_send::LiveSendTarget::Agent => crate::agents::send_keys_enter_delay(&inst.tool),
            live_send::LiveSendTarget::Terminal
            | live_send::LiveSendTarget::ContainerTerminal
            | live_send::LiveSendTarget::Tool(_) => 0,
        };
        if let Err(e) = tmux_session.send_keys_with_delay(message, delay) {
            self.info_dialog = Some(InfoDialog::new(
                "Send Failed",
                &format!("Failed to send message: {}", e),
            ));
            return;
        }
        self.stamp_last_accessed(session_id);
        if let Err(e) = self.save() {
            tracing::error!("Failed to save after send: {}", e);
        }
        if self.sort_order == crate::session::config::SortOrder::Attention {
            self.select_top_attention(None);
            self.selected_session = None;
        }
    }

    /// Send the tmux keystrokes for a permission-prompt decision straight
    /// to the selected session's agent pane. No pane-readiness wait like
    /// `execute_send_message` performs: this action only makes sense
    /// against an already-live pane showing a prompt, so there is nothing
    /// to revive.
    pub fn execute_permission_response(
        &mut self,
        session_id: &str,
        choice: crate::tui::dialogs::PermissionResponseChoice,
    ) {
        let Some(inst) = self.get_instance(session_id) else {
            return;
        };
        if inst.is_structured() {
            return;
        }
        let Some(response) =
            crate::agents::get_agent(&inst.tool).and_then(|a| a.permission_response)
        else {
            return;
        };
        let Some(tokens) = permission_response_tokens(&response, choice) else {
            return;
        };
        let tmux_session = match crate::tmux::Session::new(&inst.id, &inst.title) {
            Ok(s) => s,
            Err(e) => {
                self.info_dialog = Some(InfoDialog::new(
                    "Respond Failed",
                    &format!("Failed to resolve session: {}", e),
                ));
                return;
            }
        };
        if let Err(e) = tmux_session.send_key_tokens(tokens) {
            self.info_dialog = Some(InfoDialog::new(
                "Respond Failed",
                &format!("Failed to send response: {}", e),
            ));
        }
    }
}

#[cfg(test)]
mod permission_response_tokens_tests {
    use super::*;
    use crate::agents::{KeyToken, PermissionResponse};
    use crate::tui::dialogs::PermissionResponseChoice;

    #[test]
    fn maps_each_choice_to_its_own_field() {
        let response = PermissionResponse {
            allow: &[KeyToken::Literal("1")],
            allow_always: Some(&[KeyToken::Literal("2")]),
            deny: &[KeyToken::Literal("3")],
        };
        assert_eq!(
            permission_response_tokens(&response, PermissionResponseChoice::Allow),
            Some(response.allow)
        );
        assert_eq!(
            permission_response_tokens(&response, PermissionResponseChoice::AllowAlways),
            response.allow_always
        );
        assert_eq!(
            permission_response_tokens(&response, PermissionResponseChoice::Deny),
            Some(response.deny)
        );
    }

    #[test]
    fn allow_always_none_maps_to_none() {
        let response = PermissionResponse {
            allow: &[KeyToken::Named("Enter")],
            allow_always: None,
            deny: &[KeyToken::Named("Down"), KeyToken::Named("Enter")],
        };
        assert_eq!(
            permission_response_tokens(&response, PermissionResponseChoice::AllowAlways),
            None
        );
    }
}
