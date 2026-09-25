//! Sending a message or a permission response to the selected session.

use super::*;

/// Map a decision to its agent-defined keystroke sequence. Pure and tmux-free so the
/// mapping is unit-testable; `execute_permission_response` is the only caller.
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

    /// Stamp `last_accessed_at` on a session (a user-initiated interaction).
    ///
    /// Sunk rows take the heavier `apply_user_action` path so the auto-unarchive side effect
    /// in `touch_last_accessed` is persisted (merge_from_tui doesn't carry those fields, so
    /// a reload would resurrect the sink) and the row leaves the Archived section on the same
    /// frame. Non-sunk rows stay on the cheap mutate_instance path, since save() already
    /// mirrors the timestamp.
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

    /// Run the send-message work after the dialog is dismissed: `ensure_pane_ready` (which
    /// may auto-start or respawn), then deliver the keystrokes. Errors surface via
    /// `info_dialog`, so the caller only has to clear its transient status.
    ///
    pub fn execute_send_message(&mut self, session_id: &str, message: &str) {
        let target = std::mem::replace(
            &mut self.pending_send_target,
            live_send::LiveSendTarget::Agent,
        );
        // Same pane-readiness cascades as live-send: the agent runs the full
        // `ensure_pane_ready` while terminals just need a live pane. Every cold target starts
        // at the visible preview size, avoiding an immediate resize and its SIGWINCH
        // repaint.
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
        // The agent gets a tool-specific Enter delay so paste-burst-aware agents don't
        // swallow the final Enter; shells in the terminal panes don't need it.
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

    /// Send the tmux keystrokes for a permission-prompt decision to the selected session's
    /// agent pane. No pane-readiness wait: this only makes sense against a live pane already
    /// showing a prompt.
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
        let without_always = PermissionResponse {
            allow_always: None,
            ..response
        };
        assert_eq!(
            permission_response_tokens(&without_always, PermissionResponseChoice::AllowAlways),
            None
        );
    }
}
