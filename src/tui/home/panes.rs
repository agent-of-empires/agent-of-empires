//! Bringing a session's tmux panes up: terminal, container terminal, and
//! tool panes.

use super::*;

/// Which pane a native preparation targets: the session's own agent pane, or
/// one of its auxiliary panes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) enum NativePane {
    Agent,
    Auxiliary(crate::session::AuxiliaryTarget),
}

/// What to do once the daemon confirms the prepared pane is ready. Attaching,
/// entering live send and delivering a message share one preparation, so the
/// continuation travels with it instead of blocking the UI thread.
pub(in crate::tui) enum PaneIntent {
    Attach,
    LiveSend(crate::tui::home::live_send::LiveSendTarget),
    Send {
        message: String,
        target: crate::tui::home::live_send::LiveSendTarget,
    },
}

impl NativePane {
    /// The auxiliary target a live-send target prepares, if it is one.
    fn for_live_send(target: &crate::tui::home::live_send::LiveSendTarget) -> Self {
        use crate::tui::home::live_send::LiveSendTarget;
        match target {
            LiveSendTarget::Agent => Self::Agent,
            LiveSendTarget::Terminal => {
                Self::Auxiliary(crate::session::AuxiliaryTarget::Host { index: 0 })
            }
            LiveSendTarget::ContainerTerminal => {
                Self::Auxiliary(crate::session::AuxiliaryTarget::Container { index: 0 })
            }
            LiveSendTarget::Tool(tool_name) => {
                Self::Auxiliary(crate::session::AuxiliaryTarget::Tool {
                    tool_name: tool_name.clone(),
                })
            }
        }
    }
}

pub(super) struct PendingLegacyToolPreparation {
    id: String,
    pane: NativePane,
    size: Option<(u16, u16)>,
    intent: PaneIntent,
    adoption: crate::session::LegacyToolAdoption,
    profile_filter: Option<String>,
    view_mode: ViewMode,
    terminal_mode: TerminalMode,
}

pub(super) struct PendingNativeAttachment {
    id: String,
    profile_filter: Option<String>,
    source_profile: String,
    view_mode: ViewMode,
    terminal_mode: TerminalMode,
    intent: PaneIntent,
    preparation: crate::tui::session_feed::NativePreparation,
}

impl HomeView {
    pub(in crate::tui) fn prepare_native_attachment(
        &mut self,
        id: &str,
        pane: NativePane,
        size: Option<(u16, u16)>,
        intent: PaneIntent,
    ) -> anyhow::Result<()> {
        self.cancel_native_attachment();
        anyhow::ensure!(
            self.selected_session.as_deref() == Some(id),
            "Session is no longer selected"
        );
        if let NativePane::Auxiliary(crate::session::AuxiliaryTarget::Tool { tool_name }) = &pane {
            let row = self
                .get_instance(id)
                .ok_or_else(|| anyhow::anyhow!("Session is unknown to this view"))?;
            let observation = row.auxiliary.iter().find(|observation|
                matches!(&observation.target, crate::session::AuxiliaryTarget::Tool { tool_name: name } if name == tool_name))
                .ok_or_else(|| anyhow::anyhow!("Tool ownership has not been observed yet. Wait for the runtime snapshot."))?;
            {
                if let Some(identity) = &observation.pane.legacy_tool {
                    let adoption = crate::session::LegacyToolAdoption {
                        tmux_session: observation
                            .pane
                            .tmux_session
                            .clone()
                            .ok_or_else(|| anyhow::anyhow!("Legacy pane name unavailable"))?,
                        identity: identity.clone(),
                        profile: row.source_profile.clone(),
                        lifecycle_generation: row.lifecycle_generation,
                    };
                    let message = format!("Adopt legacy tool '{}' for '{}'\nRow: {} / {} / generation {}\nTmux: {} ({} / {}, PID {})\nConfirm ownership of this exact unmarked pane before opening it.",
                        tool_name, row.title, row.source_profile, row.id, row.lifecycle_generation,
                        adoption.tmux_session, identity.session_id, identity.pane_id, identity.pane_pid);
                    self.pending_legacy_tool_preparation = Some(PendingLegacyToolPreparation {
                        id: id.to_owned(),
                        pane,
                        size,
                        intent,
                        adoption,
                        profile_filter: self.active_profile.clone(),
                        view_mode: self.view_mode.clone(),
                        terminal_mode: self.get_terminal_mode(id),
                    });
                    self.confirm_dialog = Some(
                        ConfirmDialog::new("Adopt Legacy Tool", &message, "adopt_legacy_tool")
                            .buttons("Adopt", "Cancel"),
                    );
                    return Ok(());
                }
                anyhow::ensure!(
                    observation.pane.state != crate::session::PanePresence::Unknown,
                    "Tool ownership is unavailable, invalid, or ambiguous; no tool was opened"
                );
            }
        }
        self.prepare_native_attachment_authorized(id, pane, size, intent, None)
    }

    pub(super) fn confirm_legacy_tool_preparation(&mut self) -> anyhow::Result<()> {
        let pending = self
            .pending_legacy_tool_preparation
            .take()
            .ok_or_else(|| anyhow::anyhow!("Legacy tool confirmation is no longer available"))?;
        anyhow::ensure!(
            self.selected_session.as_deref() == Some(&pending.id)
                && self.active_profile == pending.profile_filter
                && self.view_mode == pending.view_mode
                && self.get_terminal_mode(&pending.id) == pending.terminal_mode,
            "Tool selection changed while confirmation was open"
        );
        let row = self
            .get_instance(&pending.id)
            .ok_or_else(|| anyhow::anyhow!("Session no longer exists"))?;
        anyhow::ensure!(
            row.source_profile == pending.adoption.profile
                && row.lifecycle_generation == pending.adoption.lifecycle_generation,
            "Session identity changed while confirmation was open"
        );
        self.prepare_native_attachment_authorized(
            &pending.id,
            pending.pane,
            pending.size,
            pending.intent,
            Some(pending.adoption),
        )
    }

    fn prepare_native_attachment_authorized(
        &mut self,
        id: &str,
        pane: NativePane,
        size: Option<(u16, u16)>,
        intent: PaneIntent,
        adoption: Option<crate::session::LegacyToolAdoption>,
    ) -> anyhow::Result<()> {
        self.cancel_native_attachment();
        anyhow::ensure!(
            self.selected_session.as_deref() == Some(id),
            "Session is no longer selected"
        );
        // The applied snapshot can trail by a frame; the profile the fence
        // compares comes from the row this view holds either way.
        let source_profile = self
            .session_feed
            .applied_session(id)
            .map(|row| row.profile.clone())
            .or_else(|| {
                self.get_instance(id)
                    .map(|inst| inst.source_profile.clone())
            })
            .ok_or_else(|| anyhow::anyhow!("Session is unknown to this view"))?;
        let preparation = match pane {
            NativePane::Agent => self.session_feed.ensure_agent(id.into(), size)?,
            NativePane::Auxiliary(target) => {
                self.session_feed
                    .ensure_auxiliary(id.into(), target, size, adoption)?
            }
        };
        self.pending_native_attachment = Some(PendingNativeAttachment {
            id: id.into(),
            profile_filter: self.active_profile.clone(),
            source_profile,
            view_mode: self.view_mode.clone(),
            terminal_mode: self.get_terminal_mode(id),
            intent,
            preparation,
        });
        Ok(())
    }

    /// Prepare the pane a live-send target needs, remembering that entering
    /// live send is the continuation.
    pub(in crate::tui) fn prepare_live_send_target(
        &mut self,
        id: &str,
        target: crate::tui::home::live_send::LiveSendTarget,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let pane = NativePane::for_live_send(&target);
        self.prepare_native_attachment(id, pane, size, PaneIntent::LiveSend(target))
    }

    /// Prepare the pane a message is addressed to and remember the delivery.
    pub(in crate::tui) fn prepare_send_target(
        &mut self,
        id: &str,
        target: crate::tui::home::live_send::LiveSendTarget,
        message: String,
        size: Option<(u16, u16)>,
    ) -> anyhow::Result<()> {
        let pane = NativePane::for_live_send(&target);
        self.prepare_native_attachment(id, pane, size, PaneIntent::Send { message, target })
    }

    pub(super) fn cancel_native_attachment(&mut self) {
        self.pending_legacy_tool_preparation = None;
        if let Some(pending) = self.pending_native_attachment.take() {
            pending.preparation.lease.cancel_continuation();
        }
    }

    pub(in crate::tui) fn reconcile_native_attachment(&mut self) {
        if self
            .pending_native_attachment
            .as_ref()
            .is_some_and(|pending| {
                !pending.preparation.lease.is_valid()
                    || self.selected_session.as_deref() != Some(&pending.id)
                    || self.active_profile != pending.profile_filter
                    || self.view_mode != pending.view_mode
                    || self.get_terminal_mode(&pending.id) != pending.terminal_mode
                    || self.has_non_live_send_overlay()
                    || self
                        .session_feed
                        .applied_session(&pending.id)
                        .is_none_or(|row| row.profile != pending.source_profile)
            })
        {
            self.cancel_native_attachment();
        }
    }

    pub(in crate::tui) fn take_native_attachment(&mut self) -> Option<ReadyNativeAttachment> {
        use tokio::sync::oneshot::error::TryRecvError;
        self.reconcile_native_attachment();
        let result = self
            .pending_native_attachment
            .as_mut()?
            .preparation
            .result
            .try_recv();
        if matches!(result, Err(TryRecvError::Empty)) {
            return None;
        }
        let pending = self.pending_native_attachment.take()?;
        let error = match result {
            Ok(Ok(tmux_name)) if pending.preparation.lease.is_valid() => {
                return Some(ReadyNativeAttachment {
                    id: pending.id,
                    tmux_name,
                    lease: pending.preparation.lease,
                    intent: pending.intent,
                });
            }
            Ok(Ok(_)) => return None,
            Ok(Err(error)) => error,
            Err(_) => "Runtime preparation interrupted".into(),
        };
        pending.preparation.lease.cancel_continuation();
        let title = match &pending.intent {
            PaneIntent::Attach => "Attachment failed",
            PaneIntent::LiveSend(_) => "Live send failed",
            PaneIntent::Send { .. } => "Send Failed",
        };
        self.info_dialog = Some(InfoDialog::new(title, &error));
        None
    }

    /// Attach only after this restart's receipt and its compatible snapshot.
    pub fn restart_then_attach(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
        skip_on_launch: bool,
    ) {
        self.queue_launch_then_attach(id, size, Some(skip_on_launch));
    }

    pub fn start_then_attach(&mut self, id: &str, size: Option<(u16, u16)>) {
        self.queue_launch_then_attach(id, size, None);
    }

    fn queue_launch_then_attach(
        &mut self,
        id: &str,
        size: Option<(u16, u16)>,
        restart_skip_on_launch: Option<bool>,
    ) {
        if self.restart_in_flight.contains(id) {
            return;
        }
        let Some(instance) = self.get_instance(id) else {
            return;
        };
        let source_profile = instance.source_profile.clone();
        self.cancel_native_attachment();
        let preparation = if let Some(skip_on_launch) = restart_skip_on_launch {
            let body = crate::daemon::RestartSessionBody {
                size: size.and_then(|(cols, rows)| {
                    Some(crate::daemon::TerminalSize {
                        cols: std::num::NonZeroU16::new(cols)?,
                        rows: std::num::NonZeroU16::new(rows)?,
                    })
                }),
                skip_on_launch,
                wake_message: Some(String::new()),
                ..Default::default()
            };
            self.session_feed.restart_agent(id.into(), body)
        } else {
            self.session_feed.start_agent(id.into(), size)
        };
        match preparation {
            Ok(preparation) => {
                self.pending_native_attachment = Some(PendingNativeAttachment {
                    id: id.into(),
                    profile_filter: self.active_profile.clone(),
                    source_profile,
                    view_mode: self.view_mode.clone(),
                    terminal_mode: self.get_terminal_mode(id),
                    intent: PaneIntent::Attach,
                    preparation,
                });
                self.restart_in_flight.insert(id.into());
            }
            Err(error) => {
                self.info_dialog = Some(InfoDialog::new(
                    if restart_skip_on_launch.is_some() {
                        "Restart failed"
                    } else {
                        "Start failed"
                    },
                    &format!("{error}\nReconnect the runtime and try again."),
                ))
            }
        }
    }

    /// Submit a daemon stop for id through the feed. The daemon snapshot remains authoritative.
    pub(in crate::tui) fn submit_daemon_stop_via_ui(&mut self, id: &str) -> anyhow::Result<()> {
        self.submit_daemon_stop(id).map(|_| ())
    }

    /// Submit a daemon stop and surface a refusal without mutating local state.
    pub(in crate::tui) fn submit_daemon_stop(&mut self, id: &str) -> anyhow::Result<bool> {
        match self
            .session_feed
            .submit(id.to_string(), crate::daemon::SessionMutation::Stop)
        {
            Ok(()) => Ok(true),
            Err(error) => {
                self.info_dialog = Some(InfoDialog::sized_to_fit(
                    "Stop failed",
                    &format!(
                        "Could not stop the session: {error}\nReconnect the runtime and try again."
                    ),
                ));
                Ok(false)
            }
        }
    }

    /// Get the terminal mode for a session (uses config default if not set)
    pub fn get_terminal_mode(&self, session_id: &str) -> TerminalMode {
        self.terminal_modes
            .get(session_id)
            .copied()
            .unwrap_or(self.default_terminal_mode)
    }

    /// Toggle terminal mode between Container and Host for a session
    pub fn toggle_terminal_mode(&mut self, session_id: &str) {
        self.cancel_native_attachment();
        let current = self.get_terminal_mode(session_id);
        let new_mode = match current {
            TerminalMode::Container => TerminalMode::Host,
            TerminalMode::Host => TerminalMode::Container,
        };
        self.terminal_modes.insert(session_id.to_string(), new_mode);
    }
    /// The terminal a session's Terminal view shows: its chosen mode when
    /// sandboxed, otherwise always the host terminal.
    pub(super) fn effective_terminal_mode(&self, session_id: &str) -> TerminalMode {
        match self.get_instance(session_id) {
            Some(inst) if inst.is_sandboxed() => self.get_terminal_mode(session_id),
            _ => TerminalMode::Host,
        }
    }
}
