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
        self.mutate_instance(id, |inst| inst.status = status);
    }

    /// Mark a user interaction.
    ///
    /// The daemon stamps the canonical access time; gestures never save a
    /// local row/group mirror, including gestures that unsink a row.
    pub fn stamp_last_accessed(&mut self, id: &str) {
        // A session with a change in flight is stamped by that change's own
        // commit, so a second stamp is both redundant and refused by the
        // per-session command queue.
        if self.session_feed.has_pending(id) || self.session_feed.has_queued(id) {
            return;
        }
        if let Err(error) = self
            .session_feed
            .submit(id.to_owned(), crate::daemon::SessionMutation::Access)
        {
            tracing::warn!(target: "tui.home", session_id = %id, %error, "access request refused");
        }
    }

    /// Why the session's agent must not start or take input, if it is archived or trashed.
    pub(super) fn start_blocked(&self, session_id: &str) -> Option<crate::session::StartBlocked> {
        self.get_instance(session_id)?.ensure_startable().err()
    }

    /// Take the target the next message should be delivered to.
    pub(in crate::tui) fn take_send_target(&mut self) -> live_send::LiveSendTarget {
        std::mem::replace(
            &mut self.pending_send_target,
            live_send::LiveSendTarget::Agent,
        )
    }

    /// Deliver a queued message to a pane the daemon has confirmed ready.
    pub(in crate::tui) fn finish_send(
        &mut self,
        session_id: &str,
        terminal: &std::sync::Arc<crate::daemon::TerminalTarget>,
        target: live_send::LiveSendTarget,
        message: &str,
        lease: &crate::tui::session_feed::NativeLease,
    ) {
        if !lease.is_valid() {
            return;
        }
        if !self.session_feed.native_interaction_available() {
            self.info_dialog = Some(InfoDialog::new(
                "Send Failed",
                "Native interaction is unavailable.",
            ));
            return;
        }
        // An archived or trashed agent takes no input, even with a live pane
        // (#4118). The runtime refuses the prepare that produced the receipt,
        // so this keeps the refusal on the delivery path itself; the row it
        // reads is the runtime's committed snapshot.
        if matches!(target, live_send::LiveSendTarget::Agent) {
            if let Some(blocked) = self.start_blocked(session_id) {
                self.info_dialog = Some(InfoDialog::new("Send Failed", &blocked.to_string()));
                return;
            }
        }
        let Some(inst) = self.get_instance(session_id).cloned() else {
            self.info_dialog = Some(InfoDialog::new(
                "Send Failed",
                "Session disappeared before the message could be sent.",
            ));
            return;
        };
        let agent = matches!(target, live_send::LiveSendTarget::Agent);
        // Agent gets a tool-specific Enter delay so paste-burst-aware
        // agents (e.g. Codex) don't swallow the final Enter. Shells in
        // the paired terminal panes don't need the delay.
        let delay = match &target {
            live_send::LiveSendTarget::Agent => crate::agents::send_keys_enter_delay(&inst.tool),
            live_send::LiveSendTarget::Terminal
            | live_send::LiveSendTarget::ContainerTerminal
            | live_send::LiveSendTarget::Tool(_) => 0,
        };
        let delivered = (|| -> anyhow::Result<()> {
            anyhow::ensure!(
                lease
                    .target()
                    .is_some_and(|owned| std::sync::Arc::ptr_eq(owned, terminal))
                    && inst.lifecycle_generation == terminal.lifecycle_generation
                    && inst.effective_profile() == terminal.profile,
                crate::session::NativeInputError::Ownership
            );
            let physical = terminal
                .physical
                .clone()
                .ok_or(crate::session::NativeInputError::Ownership)?;
            let fence = lease.input_fence(agent)?;
            let session = if agent {
                inst.agent_input_session(&terminal.tmux_session, physical, true, Some(fence))?
            } else {
                crate::tmux::Session::from_observation(&terminal.tmux_session, physical)
                    .with_native_input_fence(fence)
            };
            anyhow::ensure!(
                lease.is_valid(),
                crate::session::NativeInputError::Ownership
            );
            session.send_keys_with_delay(message, delay)
        })();
        if let Err(error) = delivered {
            self.info_dialog = Some(InfoDialog::new("Send Failed", &error.to_string()));
            return;
        }
        #[cfg(test)]
        post_send_test_hook::run(session_id);
        if let Err(error) = self.session_feed.submit(
            session_id.to_owned(),
            crate::daemon::SessionMutation::InputRecency(crate::daemon::InputRecencyBody {
                created_at: inst.created_at,
                lifecycle_generation: terminal.lifecycle_generation,
                profile: terminal.profile.clone(),
                profile_identity: lease.preparation_profile_identity(),
            }),
        ) {
            tracing::warn!(target: "tui.home", session_id, %error, "input recency request refused");
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
        if !self.session_feed.native_interaction_available() {
            self.info_dialog = Some(InfoDialog::new(
                "Respond Failed",
                "Native interaction is unavailable.",
            ));
            return;
        }
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
        let delivered = (|| -> anyhow::Result<()> {
            let pane = &inst.agent_pane;
            anyhow::ensure!(
                pane.state == crate::session::PanePresence::Alive,
                crate::session::NativeInputError::Ownership
            );
            let name = pane
                .tmux_session
                .as_deref()
                .ok_or(crate::session::NativeInputError::Ownership)?;
            let physical = pane
                .physical
                .clone()
                .ok_or(crate::session::NativeInputError::Ownership)?;
            let session = inst.agent_input_session(name, physical, false, None)?;
            session.send_key_tokens(tokens)
        })();
        if let Err(e) = delivered {
            self.info_dialog = Some(InfoDialog::new(
                "Respond Failed",
                &format!("Failed to send response: {}", e),
            ));
        }
    }
}

#[cfg(test)]
pub(super) mod post_send_test_hook {
    use std::cell::RefCell;
    use std::collections::HashMap;

    thread_local! {
        static HOOKS: RefCell<HashMap<String, Box<dyn FnOnce()>>> = RefCell::new(HashMap::new());
    }

    pub(in crate::tui::home) struct Guard(String);

    impl Drop for Guard {
        fn drop(&mut self) {
            HOOKS.with(|hooks| hooks.borrow_mut().remove(&self.0));
        }
    }

    pub(in crate::tui::home) fn install(id: &str, hook: impl FnOnce() + 'static) -> Guard {
        HOOKS.with(|hooks| {
            assert!(hooks
                .borrow_mut()
                .insert(id.into(), Box::new(hook))
                .is_none());
        });
        Guard(id.into())
    }

    pub(super) fn run(id: &str) {
        let hook = HOOKS.with(|hooks| hooks.borrow_mut().remove(id));
        if let Some(hook) = hook {
            hook();
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
