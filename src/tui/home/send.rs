//! Sending a message or a permission response to the selected session.

use super::*;

/// Map a decision to its agent-defined keystroke sequence. Pure and
/// tmux-free so the choice-to-field mapping is unit-testable without a
/// real pane; `execute_permission_response` is the only caller.
pub(super) fn permission_response_tokens<'a>(
    response: &crate::agents::PermissionResponse<'a>,
    choice: crate::tui::dialogs::PermissionResponseChoice,
) -> Option<&'a [crate::agents::KeyToken<'a>]> {
    use crate::tui::dialogs::PermissionResponseChoice::*;
    match choice {
        Allow => Some(response.allow),
        AllowAlways => response.allow_always,
        Deny => Some(response.deny),
    }
}

/// Types `message` into the session's live pane, resolved once so the draft check and the keys
/// address the same pane. `Err` is the dialog text.
fn deliver_to_live_pane(
    inst: &crate::session::Instance,
    tmux: &crate::tmux::Session,
    check_draft: bool,
    message: &str,
    delay: u64,
    validate: impl FnOnce() -> anyhow::Result<()>,
) -> Result<(), String> {
    let failed = |e: anyhow::Error| format!("Failed to send message: {e}");
    let pane = tmux
        .live_pane_target_with_deadline(&crate::tmux::TmuxCommandDeadline::new())
        .map_err(failed)?;
    // Checked once: a wait here would freeze the TUI, and the person can see the pane.
    if check_draft && inst.input_line_in(tmux, &pane) == crate::tmux::InputLine::Draft {
        return Err(crate::tmux::InputLine::BUSY_MESSAGE.to_string());
    }
    #[cfg(test)]
    crate::tmux::test_helpers::after_input_check(&pane);
    validate().map_err(failed)?;
    tmux.send_keys_to_pane(&pane, message, delay)
        .map_err(failed)
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

    /// Shows why a send was refused at delivery, keeping its text for the dialog that reopens.
    pub(super) fn refuse_send(
        &mut self,
        session_id: &str,
        target: live_send::LiveSendTarget,
        title: &str,
        message: &str,
        refusal: &str,
    ) {
        self.info_dialog = Some(InfoDialog::new("Send Failed", refusal));
        self.refused_send = Some((
            session_id.to_owned(),
            target,
            title.to_owned(),
            message.to_owned(),
        ));
    }

    /// Closes the info dialog, then reopens a send it refused at delivery with the same text.
    pub(super) fn dismiss_info_dialog(&mut self) {
        self.info_dialog = None;
        if let Some((session_id, target, title, message)) = self.refused_send.take() {
            let label = live_send::format_target_label(&title, &target);
            self.pending_send_session = Some(session_id);
            self.pending_send_target = target;
            let mut dialog = crate::tui::dialogs::SendMessageDialog::new(&label);
            dialog.handle_paste(&message);
            self.send_message_dialog = Some(dialog);
        }
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
            live_send::LiveSendTarget::Agent => inst.send_keys_enter_delay(),
            live_send::LiveSendTarget::Terminal
            | live_send::LiveSendTarget::ContainerTerminal
            | live_send::LiveSendTarget::Tool(_) => 0,
        };
        let session = (|| -> anyhow::Result<_> {
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
            if agent {
                inst.agent_input_session(&terminal.tmux_session, physical, true, Some(fence))
            } else {
                Ok(
                    crate::tmux::Session::from_observation(&terminal.tmux_session, physical)
                        .with_native_input_fence(fence),
                )
            }
        })();
        let delivered = session
            .map_err(|error| error.to_string())
            .and_then(|session| {
                deliver_to_live_pane(&inst, &session, agent, message, delay, || {
                    anyhow::ensure!(
                        lease.is_valid(),
                        crate::session::NativeInputError::Ownership
                    );
                    Ok(())
                })
            });
        if let Err(refusal) = delivered {
            self.refuse_send(session_id, target, &inst.title, message, &refusal);
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
        if let Some(blocked) = self.start_blocked(session_id) {
            self.info_dialog = Some(InfoDialog::new("Respond Failed", &blocked.to_string()));
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

    pub(super) fn execute_declared_permission_response(
        &mut self,
        session_id: &str,
        choice: crate::tui::dialogs::PermissionResponseChoice,
        agent: &crate::plugin::agent_integration::DeclaredAgent,
    ) {
        if !self.session_feed.native_interaction_available() {
            self.info_dialog = Some(InfoDialog::new(
                "Respond Failed",
                "Native interaction is unavailable.",
            ));
            return;
        }
        if let Some(blocked) = self.start_blocked(session_id) {
            self.info_dialog = Some(InfoDialog::new("Respond Failed", &blocked.to_string()));
            return;
        }
        let Some(inst) = self.get_instance(session_id) else {
            self.info_dialog = Some(InfoDialog::new(
                "Respond Failed",
                "Session no longer exists.",
            ));
            return;
        };
        if inst.is_structured()
            || crate::plugin::agent_integration::replies_configured(inst)
            || !inst
                .declared_agent()
                .is_some_and(|current| current.same_identity(agent))
        {
            self.info_dialog = Some(InfoDialog::new(
                "Response Not Sent",
                "The agent declaration changed or no longer permits this response.",
            ));
            return;
        }
        let tmux = match (|| -> anyhow::Result<crate::tmux::Session> {
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
            inst.agent_input_session(name, physical, false, None)
        })() {
            Ok(session) => session,
            Err(error) => {
                self.info_dialog = Some(InfoDialog::new("Respond Failed", &error.to_string()));
                return;
            }
        };
        let result = agent
            .while_current(|| {
                agent.with_definition(|definition| {
                    let response = definition.permission_response?;
                    let tokens = permission_response_tokens(&response, choice)?;
                    Some(tmux.send_key_tokens(tokens))
                })
            })
            .flatten()
            .flatten();
        match result {
            Some(Ok(())) => {}
            Some(Err(error)) => {
                self.info_dialog = Some(InfoDialog::new("Respond Failed", &error.to_string()));
            }
            None => {
                self.info_dialog = Some(InfoDialog::new(
                    "Response Not Sent",
                    "The agent declaration changed or no longer permits this response.",
                ));
            }
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

#[cfg(all(test, unix))]
mod deliver_to_live_pane_tests {
    use super::*;
    use crate::tmux::test_helpers::{
        on_input_checked, on_text_typed, only_pane_id, split_pane, TmuxTestSession,
    };

    fn pane_shows(pane: &str, text: &str) -> bool {
        crate::tmux::tmux_command()
            .args(["capture-pane", "-p", "-t", pane])
            .output()
            .is_ok_and(|out| String::from_utf8_lossy(&out.stdout).contains(text))
    }

    /// The pane's screen; a failed capture fails the test rather than reading as nothing typed.
    fn captured(pane: &str) -> String {
        let out = crate::tmux::tmux_command()
            .args(["capture-pane", "-p", "-t", pane])
            .output()
            .expect("capture-pane");
        assert!(
            out.status.success(),
            "capture {pane}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Whether any session on the server lists `pane`.
    fn pane_listed(pane: &str) -> bool {
        let out = crate::tmux::tmux_command()
            .args(["list-panes", "-a", "-F", "#{pane_id}"])
            .output()
            .expect("list-panes");
        assert!(
            out.status.success(),
            "list-panes: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .any(|line| line == pane)
    }

    fn lines_of(pane: &str) -> Vec<String> {
        captured(pane)
            .lines()
            .map(|line| line.trim().to_string())
            .collect()
    }

    fn start_cat(session: &TmuxTestSession) {
        let mut args: Vec<String> = ["new-session", "-d", "-s", session.name(), "sh", "-c"]
            .map(String::from)
            .into();
        args.push("exec cat".into());
        crate::tmux::utils::append_pane_base_index_args(&mut args, session.name());
        assert!(crate::tmux::tmux_command()
            .args(args)
            .status()
            .unwrap()
            .success());
    }

    fn wait_for(case: &str, what: &str, done: impl Fn() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done() {
            assert!(std::time::Instant::now() < deadline, "{case}: {what}");
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    /// The keys reach the pane the draft check read, or fail, even when the live target moves
    /// to another pane between the check and the send, or the checked pane moves to another
    /// session.
    #[test]
    fn a_send_types_into_the_pane_it_checked() {
        if !crate::tmux::is_tmux_available() {
            return;
        }
        #[derive(Clone, Copy, PartialEq)]
        enum Then {
            /// The pin is dropped after the check.
            Unpin,
            /// The checked pane is killed as well.
            Kill,
            /// The checked pane is moved into another session after the check.
            Move,
            /// The checked pane is moved into another session after its text is typed.
            MoveBeforeEnter,
        }
        for (case, then) in [
            ("unpinned", Then::Unpin),
            ("killed", Then::Kill),
            ("moved", Then::Move),
            ("moved-before-enter", Then::MoveBeforeEnter),
        ] {
            let session = TmuxTestSession::new("tui_send");
            start_cat(&session);
            let other = TmuxTestSession::new("tui_send_other");
            start_cat(&other);
            let first = only_pane_id(session.name());
            let checked = split_pane(session.name(), "exec cat");
            let dir = tempfile::tempdir().unwrap();
            let pin = crate::tmux::vt::register_live_target_for_test(
                session.name(),
                dir.path(),
                &checked,
            );
            let move_pane = {
                let (pane, to) = (checked.clone(), format!("={}:", other.name()));
                move || {
                    assert!(crate::tmux::tmux_command()
                        .args(["join-pane", "-d", "-s", &pane, "-t", &to])
                        .status()
                        .unwrap()
                        .success());
                }
            };
            let target = checked.clone();
            on_input_checked(&checked, move || {
                drop(pin);
                match then {
                    Then::Kill => assert!(crate::tmux::tmux_command()
                        .args(["kill-pane", "-t", &target])
                        .status()
                        .unwrap()
                        .success()),
                    Then::Move => move_pane(),
                    Then::MoveBeforeEnter => on_text_typed(&target, move_pane),
                    Then::Unpin => {}
                }
            });
            let inst = crate::session::Instance::new("tui-send", "/tmp");
            let tmux = crate::tmux::Session::from_name(session.name());

            let sent = deliver_to_live_pane(&inst, &tmux, true, "hello", 0, || Ok(()));

            match then {
                Then::Unpin => {
                    assert_eq!(sent, Ok(()), "{case}");
                    wait_for(case, "never typed", || pane_shows(&checked, "hello"));
                }
                Then::Kill => {
                    assert!(sent.is_err(), "{case}: {sent:?}");
                    assert!(!pane_listed(&checked), "{case}: still there");
                }
                Then::Move => {
                    assert!(sent.is_err(), "{case}: {sent:?}");
                    assert!(
                        !captured(&checked).contains("hello"),
                        "{case}: typed after the move"
                    );
                }
                Then::MoveBeforeEnter => {
                    let err = sent.expect_err(case);
                    assert!(
                        err.contains("did not submit") && err.contains(&checked),
                        "{case}: {err}"
                    );
                    wait_for(case, "never typed", || pane_shows(&checked, "hello"));
                    // cat echoes a line twice once it is submitted: the tty echo and its own
                    // output. The sentinel's two lines prove any Enter of ours was handled.
                    assert!(crate::tmux::tmux_command()
                        .args(["send-keys", "-t", &checked, "-l", "sentinel"])
                        .status()
                        .unwrap()
                        .success());
                    assert!(crate::tmux::tmux_command()
                        .args(["send-keys", "-t", &checked, "Enter"])
                        .status()
                        .unwrap()
                        .success());
                    wait_for(case, "sentinel never submitted", || {
                        lines_of(&checked)
                            .iter()
                            .filter(|line| line.ends_with("sentinel"))
                            .count()
                            == 2
                    });
                    let lines = lines_of(&checked);
                    assert!(
                        !lines.iter().any(|line| line == "hello"),
                        "{case}: Enter was sent: {lines:?}"
                    );
                    assert_eq!(
                        lines.iter().filter(|line| *line == "hellosentinel").count(),
                        2,
                        "{case}: {lines:?}"
                    );
                }
            }
            assert!(
                !captured(&first).contains("hello"),
                "{case}: typed into the other pane"
            );
        }
    }
}
