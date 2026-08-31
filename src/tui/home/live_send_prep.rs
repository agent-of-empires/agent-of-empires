//! Getting a pane warm enough to take live input, and the resize that
//! follows.

use super::*;

impl HomeView {
    /// Whether the agent row is in a live status with its tmux pane up, so a
    /// revive cascade (`ensure_pane_ready` / `prepare_live_send`) is expected
    /// to be a fast no-op.
    ///
    /// The `EnterLiveSend` / `SendMessage` handlers use this to skip the
    /// "Reviving session..." toast frame for warm sessions: the toast claims a
    /// bottom bar row, and for the frame(s) it is on screen the bottom-anchored
    /// preview paints its content one row up (68 cached rows into 67 visible),
    /// then drops back when the toast clears. On a warm entry that hop is the
    /// only thing the toast ever shows the user; how long it lingers depends on
    /// how slow the readiness re-checks happen to be, which is why it reads as
    /// an intermittent "cursor jiggle" on live-view entry. Cold paths (dead
    /// pane, Docker start, agent splash) keep the toast: there the feedback is
    /// real and the reflow unavoidable.
    ///
    /// `exists()` is cache-backed, so a stale cache can misclassify a
    /// just-died pane as warm; the only cost is a missing toast over a
    /// slower-than-expected revive, never a broken entry.
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

    /// Whether the target pane is already live, so its entry can skip the
    /// transient revive toast. Terminal and tool targets check their own pane's
    /// existence and ignore the agent's status, because a stopped agent can
    /// have a live paired terminal.
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

    /// Size to boot a cold/dead pane at on live-send entry: the visible
    /// preview output rect when known, else the full terminal. Seeding the boot
    /// here avoids an initial reflow; any post-toast geometry change is queued
    /// through the size-owning worker. Falls back to the terminal size for the
    /// rare entry with no prior preview frame, and to `None` if neither is
    /// available so tmux keeps its default.
    pub(super) fn live_send_boot_size(&self) -> Option<(u16, u16)> {
        let pane = self.preview_pane_area;
        if pane.width > 0 && pane.height > 0 {
            Some((pane.width, pane.height))
        } else {
            // A zero-dimension terminal size is as unusable as no size at all;
            // drop it so the start path keeps tmux's default instead of being
            // handed `-x 0`/`-y 0`.
            crate::terminal::get_size().filter(|(cols, rows)| *cols > 0 && *rows > 0)
        }
    }

    /// Stage live-send mode against `session_id`. Mirrors
    /// `execute_send_message`'s revive cascade so a cold-start (Docker
    /// pull, agent splash) is handled before the user starts typing,
    /// then installs `live_send` state so subsequent keystrokes are
    /// captured by `handle_live_send_key`.
    ///
    /// Geometry is settled by the caller's post-toast draw. Render queues the
    /// final `preview_pane_area` through `LiveSendWorker`, which verifies
    /// size ownership before resizing; this preparation path never waits on
    /// tmux merely to align the first frame.
    ///
    /// Returns `Err(())` if the pane could not be readied (`info_dialog` is
    /// set with the underlying error so the caller only has to clear its toast).
    pub fn prepare_live_send(&mut self, session_id: &str) -> Result<(), ()> {
        let target = std::mem::replace(
            &mut self.pending_live_send_target,
            live_send::LiveSendTarget::Agent,
        );
        // Agent targets revive the agent pane via the full
        // ensure_pane_ready cascade (Docker, splash, resume). Terminal
        // targets are simpler: the paired terminal is a plain shell,
        // so we just ensure the tmux session exists and re-spawn it if
        // the pane has died (matches `attach_terminal`).
        //
        // Boot every target at the size it will be shown at, not tmux's 80x24
        // default. The first post-toast draw sends any settled geometry change
        // through the size-owning worker, so startup never races an unowned
        // synchronous resize. See `Instance::ensure_pane_ready_with_size`.
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
                // Defensive: ensure_pane_ready succeeded but the
                // instance is gone (deleted by a peer process between
                // those two calls). Without a dialog the user would
                // press Tab and see nothing happen, with no clue why.
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
        // Switching live mode from session A to session B (click on a
        // different row while already live): we need to drop the old
        // worker BEFORE resetting the old session's window-size,
        // otherwise any `Resize` still queued in the old worker can
        // fire after the reset and flip the old pane back to manual
        // sizing. The worker thread is intentionally not joined, so
        // dropping its `Sender` is the only way to know its dispatch
        // loop has finished (its `recv` returns Err and the thread
        // exits on the next iteration).
        let prev_tmux_name = self
            .live_send
            .as_ref()
            .map(|state| state.tmux_name.clone())
            .filter(|name| name != &tmux_name);
        if prev_tmux_name.is_some() {
            // Drop worker first so its queued resizes (if any) drain
            // against the old session before we reset its sizing.
            self.live_send_worker = None;
            // The capture worker is retargeted by the render reconcile, not
            // here; but drop the previous session's cached previews so the
            // first frames after the switch don't paint session A's content
            // under session B's header while B's capture worker spins up.
            // (The synchronous path got this for free via its cross-session
            // kill-switch branch; the worker path applies content lazily,
            // so clear it explicitly here.) All targets are cleared because
            // a live-send switch can retarget to Terminal / ContainerTerminal
            // too, and the view can be flipped to any of them right after.
            self.preview_cache = PreviewCache::default();
            self.terminal_preview_cache = PreviewCache::default();
            self.container_terminal_preview_cache = PreviewCache::default();
            self.tool_preview_cache = PreviewCache::default();
            if let Some(name) = &prev_tmux_name {
                crate::tmux::Session::from_name(name).reset_size_to_latest_client();
            }
        }
        // Parse the configured exit-chord list now so the per-keystroke
        // dispatch path doesn't re-parse on every event. Config edits
        // during live mode aren't possible (settings_view participates
        // in has_dialog and lives in its own takeover), so a snapshot
        // at entry time is sufficient.
        let resolved_config = resolve_config_or_warn(&self.config_profile());
        let exit_chord_spec = resolved_config.session.live_send_exit_chord;
        let exit_chords = live_send::parse_chord_list(&exit_chord_spec);
        // The leader is a single chord, not a list. An empty configured
        // value disables it (so every key, including the default `C-b`,
        // passes straight through). A non-empty but unparseable value is
        // treated as a typo and falls back to the default leader rather
        // than silently dropping the feature, mirroring how the exit
        // chord recovers from a bad spec.
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
        // Ensure the long-lived preview capture worker exists so we can hand
        // its waker to the send worker below. The worker isn't otherwise
        // spawned here (it follows the displayed pane for every view, not
        // just agent live-send, and is (re)targeted and retuned by
        // `sync_preview_capture_worker` on the next render); but it's already
        // running whenever a session was previewed before live-send entry,
        // which is the common path. Spawning it now closes the rare cold gap.
        if self.preview_capture_worker.is_none() {
            self.preview_capture_worker = Some(live_send::LiveCaptureWorker::spawn(
                self.preview_wake.clone(),
            ));
        }
        // Nudge the capture worker right after each dispatched keystroke
        // batch so typed echo is captured immediately instead of waiting up
        // to a full fast-cadence cycle. This keeps echo latency tied to
        // actual input rather than the background capture phase.
        let capture_wake = self
            .preview_capture_worker
            .as_ref()
            .map(live_send::LiveCaptureWorker::waker);
        // Spawn the background worker that dispatches translated
        // keystrokes as one-shot `tmux send-keys` subprocesses (the
        // pre-#1485 path; control-mode was tried as an optimization
        // but turned out to be unreliable on real-world tmux setups
        // and was removed in favor of this simpler model).
        self.live_send_worker = Some(live_send::LiveSendWorker::spawn(tmux_name, capture_wake));
        // Start every live-mode entry (including a switch from another
        // session) with a disarmed leader menu, so a half-entered chord
        // can't carry over from a prior target.
        self.live_send_pending_leader = false;
        // The first post-toast draw queues the settled geometry through the
        // size-owning worker, even when a prior session used the same size.
        self.live_send_last_resize = None;
        self.live_send_resize_retry_at = None;
        // Live mode takes over the pane's size from here; drop the non-live
        // preview dedup so exiting re-asserts the preview geometry cleanly.
        self.preview_pane_synced = None;
        self.stamp_last_accessed(session_id);
        Ok(())
    }
}
