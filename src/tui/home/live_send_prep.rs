//! Getting a pane warm enough to take live input, and the resize that
//! follows.

use super::*;

impl HomeView {
    /// Whether the agent row is in a live status with its pane up, so a revive cascade
    /// (`ensure_pane_ready` / `prepare_live_send`) is expected to be a fast no-op.
    ///
    /// The `EnterLiveSend` / `SendMessage` handlers use it to skip the "Reviving session..."
    /// toast for warm sessions: the toast claims a bottom row, so while it shows, the
    /// bottom-anchored preview paints one row up and drops back when it clears, which on a
    /// warm entry is the only thing the toast ever shows and reads as a cursor jiggle. Cold
    /// paths (dead pane, Docker start, agent splash) keep the toast.
    ///
    /// `exists()` is cache-backed, so a stale cache can call a just-died pane warm; the
    /// only cost is a missing toast over a slow revive, never a broken entry.
    pub fn agent_pane_is_warm(&self, session_id: &str) -> bool {
        let Some(inst) = self.get_instance(session_id) else {
            return false;
        };
        if !matches!(
            inst.status,
            crate::session::Status::Running
                | crate::session::Status::Waiting
                | crate::session::Status::Idle
        ) {
            return false;
        }
        inst.tmux_session().is_ok_and(|s| s.exists())
    }

    /// Whether the target pane is already live, so entry can skip the revive toast.
    /// Terminal and tool targets check their own pane and ignore the agent's status,
    /// because a stopped agent can have a live paired terminal.
    fn target_pane_is_warm(&self, session_id: &str, target: &live_send::LiveSendTarget) -> bool {
        let Some(inst) = self.get_instance(session_id) else {
            return false;
        };
        let tmux_name = match target {
            live_send::LiveSendTarget::Agent => return self.agent_pane_is_warm(session_id),
            live_send::LiveSendTarget::Terminal => {
                crate::tmux::TerminalSession::resolve_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                crate::tmux::ContainerTerminalSession::resolve_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::Tool(name) => {
                crate::tmux::ToolSession::new(&inst.id, &inst.title, name)
                    .session_name()
                    .to_string()
            }
        };
        crate::tmux::Session::from_name(&tmux_name).exists()
    }

    pub fn live_entry_is_warm(&self, session_id: &str) -> bool {
        self.target_pane_is_warm(session_id, &self.pending_live_send_target)
    }

    pub fn send_entry_is_warm(&self, session_id: &str) -> bool {
        self.target_pane_is_warm(session_id, &self.pending_send_target)
    }

    /// Size to boot a cold pane at on live-send entry: the visible preview output rect when
    /// known, else the full terminal, so there is no initial reflow; any post-toast geometry
    /// change is queued through the size-owning worker. `None` when neither is available, so
    /// tmux keeps its default.
    pub(super) fn live_send_boot_size(&self) -> Option<(u16, u16)> {
        let pane = self.preview_pane_area;
        if pane.width > 0 && pane.height > 0 {
            Some((pane.width, pane.height))
        } else {
            // A zero-dimension terminal size is as unusable as none, so drop it and let
            // the start path keep tmux's default rather than be handed `-x 0`.
            crate::terminal::get_size().filter(|(cols, rows)| *cols > 0 && *rows > 0)
        }
    }

    /// Stage live-send mode against `session_id`, mirroring `execute_send_message`'s revive
    /// cascade so a cold start is handled before the user types, then installing
    /// `live_send` state so later keystrokes reach `handle_live_send_key`.
    ///
    /// Geometry is settled by the caller's post-toast draw: render queues the final
    /// `preview_pane_area` through `LiveSendWorker`, which verifies size ownership first, so
    /// this path never waits on tmux to align the first frame.
    ///
    /// `Err(())` when the pane could not be readied, with `info_dialog` already set so the
    /// caller only has to clear its toast.
    pub fn prepare_live_send(&mut self, session_id: &str) -> Result<(), ()> {
        let target = std::mem::replace(
            &mut self.pending_live_send_target,
            live_send::LiveSendTarget::Agent,
        );
        // Agent targets revive through the full ensure_pane_ready cascade (Docker, splash,
        // resume); terminal targets just ensure the tmux session exists and re-spawn a dead
        // pane, matching `attach_terminal`.
        //
        // Boot every target at the size it will be shown at rather than tmux's 80x24: the
        // first post-toast draw sends any settled geometry change through the size-owning
        // worker, so startup never races an unowned synchronous resize.
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
                            "Live send failed",
                            &format!("Resume failed for sid {sid}; preserved for explicit retry"),
                        ));
                        return Err(());
                    }
                    Ok(_) => {}
                    Err(err) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Live send failed",
                            &format!("Cannot prepare session: {}", err),
                        ));
                        return Err(());
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => {
                if let Err(e) = self.ensure_terminal_pane_ready(session_id, boot_size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Live send failed",
                        &format!("Cannot prepare terminal: {}", e),
                    ));
                    return Err(());
                }
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                if let Err(e) = self.ensure_container_terminal_pane_ready(session_id, boot_size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Live send failed",
                        &format!("Cannot prepare container terminal: {}", e),
                    ));
                    return Err(());
                }
            }
            live_send::LiveSendTarget::Tool(name) => {
                let name = name.clone();
                if let Err(e) = self.ensure_tool_pane_ready(session_id, &name, boot_size) {
                    self.info_dialog = Some(InfoDialog::new(
                        "Live send failed",
                        &format!("Cannot prepare tool '{}': {}", name, e),
                    ));
                    return Err(());
                }
            }
        };
        let inst = match self.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => {
                // Defensive: ensure_pane_ready succeeded but the instance is gone (a peer
                // deleted it between the two calls). Without a dialog the user would press
                // Tab and see nothing happen.
                self.info_dialog = Some(InfoDialog::new(
                    "Live send failed",
                    "Session disappeared before live mode could start.",
                ));
                return Err(());
            }
        };
        // Resolve the tmux session name up front so the worker thread
        // can reconstruct a Session without re-touching HomeView.
        let tmux_name = match &target {
            live_send::LiveSendTarget::Agent => {
                match crate::tmux::Session::new(&inst.id, &inst.title) {
                    Ok(s) => s.name().to_string(),
                    Err(e) => {
                        self.info_dialog = Some(InfoDialog::new(
                            "Live send failed",
                            &format!("Cannot resolve tmux session: {}", e),
                        ));
                        return Err(());
                    }
                }
            }
            live_send::LiveSendTarget::Terminal => {
                crate::tmux::TerminalSession::resolve_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                crate::tmux::ContainerTerminalSession::resolve_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::Tool(name) => {
                crate::tmux::ToolSession::new(&inst.id, &inst.title, name)
                    .session_name()
                    .to_string()
            }
        };
        // Switching live mode from one session to another must drop the old worker BEFORE
        // resetting the old session's window-size, or a `Resize` still queued there can fire
        // after the reset and flip the old pane back to manual sizing. The thread is not
        // joined, so dropping its `Sender` is the only way to know its dispatch loop has
        // finished.
        let prev_tmux_name = self
            .live_send
            .as_ref()
            .map(|state| state.tmux_name.clone())
            .filter(|name| name != &tmux_name);
        if prev_tmux_name.is_some() {
            // Drop worker first so its queued resizes (if any) drain
            // against the old session before we reset its sizing.
            self.live_send_worker = None;
            // The render reconcile retargets the capture worker, but drop the previous
            // session's cached previews here so the first frames after the switch don't
            // paint session A's content under session B's header while B's worker spins up.
            // All targets are cleared, since a switch can retarget to Terminal or
            // ContainerTerminal and the view can flip to any of them right after.
            self.preview_cache = PreviewCache::default();
            self.terminal_preview_cache = PreviewCache::default();
            self.container_terminal_preview_cache = PreviewCache::default();
            self.tool_preview_cache = PreviewCache::default();
            if let Some(name) = &prev_tmux_name {
                crate::tmux::Session::from_name(name).reset_size_to_latest_client();
            }
        }
        // Parse the configured exit-chord list now so the per-keystroke path doesn't
        // re-parse on every event. Config cannot be edited during live mode (settings_view
        // participates in has_dialog), so an entry-time snapshot is sufficient.
        let resolved_config = resolve_config_or_warn(&self.config_profile());
        let exit_chord_spec = resolved_config.session.live_send_exit_chord;
        let exit_chords = live_send::parse_chord_list(&exit_chord_spec);
        // The leader is a single chord, not a list. An empty value disables it, so every
        // key passes through; an unparseable value is treated as a typo and falls back to
        // the default rather than silently dropping the feature, like the exit chord.
        let leader_spec = resolved_config.session.live_send_leader;
        let leader = if leader_spec.trim().is_empty() {
            None
        } else {
            live_send::parse_chord(&leader_spec).or_else(|| {
                tracing::warn!(
                    "live-send: unparseable leader chord '{}'; falling back to default '{}'",
                    leader_spec,
                    live_send::DEFAULT_LEADER
                );
                live_send::parse_chord(live_send::DEFAULT_LEADER)
            })
        };
        self.live_send = Some(live_send::LiveSendState {
            session_id: inst.id.clone(),
            title: inst.title.clone(),
            tmux_name: tmux_name.clone(),
            target,
            exit_chords,
            leader,
        });
        // Entering live-send means the user is now viewing this session, so
        // clear any unread marker.
        self.clear_unread_on_view(&inst.id);
        // Ensure the long-lived preview capture worker exists so its waker can go to the
        // send worker below. It is not otherwise spawned here (it follows the displayed pane
        // for every view and is retargeted by `sync_preview_capture_worker` on the next
        // render), but it is already running whenever a session was previewed before entry;
        // spawning now closes the rare cold gap.
        if self.preview_capture_worker.is_none() {
            self.preview_capture_worker = Some(live_send::LiveCaptureWorker::spawn(
                self.preview_wake.clone(),
            ));
        }
        // Nudge the capture worker after each dispatched batch so typed echo is captured
        // immediately rather than a full fast-cadence cycle later.
        let capture_wake = self
            .preview_capture_worker
            .as_ref()
            .map(live_send::LiveCaptureWorker::waker);
        // Spawn the background worker that dispatches translated keystrokes as one-shot
        // `tmux send-keys` subprocesses; control-mode was tried (#1485) and proved
        // unreliable on real tmux setups.
        self.live_send_worker = Some(live_send::LiveSendWorker::spawn(tmux_name, capture_wake));
        // Start every live-mode entry, including a switch from another session, with a
        // disarmed leader menu so a half-entered chord can't carry over.
        self.live_send_pending_leader = false;
        // The first post-toast draw queues the settled geometry through the
        // size-owning worker, even when a prior session used the same size.
        self.live_send_last_resize = None;
        self.live_send_resize_retry_at = None;
        // Live mode takes over the pane's size from here, so drop the non-live resize
        // bookkeeping and let exiting re-assert the preview geometry cleanly.
        self.clear_preview_pane_sync(session_id);
        self.stamp_last_accessed(session_id);
        Ok(())
    }
}
