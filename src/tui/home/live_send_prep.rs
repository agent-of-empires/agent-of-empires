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
    /// preview output rect when known, else the full terminal. `preview_pane_area`
    /// is the exact rect `finalize_live_send_resize` resizes to, so seeding the
    /// boot here makes the post-boot resize a no-op for cold starts (no reflow,
    /// no SIGWINCH race). Falls back to the terminal size for the rare entry
    /// with no prior preview frame (e.g. attach-on-create), and to `None` if
    /// neither is available so tmux keeps its default.
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
    /// Geometry-sensitive work is intentionally split out into
    /// `finalize_live_send_resize`: the caller is expected to settle
    /// any toast/banner state (which can shift `preview_pane_area` by a
    /// row) and redraw between `prepare_live_send` and
    /// `finalize_live_send_resize`, so the sync resize targets the
    /// geometry the user will actually see for the next several frames.
    /// Without that split, the "Reviving session..." toast shown during
    /// this slow phase made `preview_pane_area` one row shorter than
    /// the post-toast frame, and the agent's first capture rendered
    /// shifted up.
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
        // default. A cold-started pane that boots narrow relies on
        // `finalize_live_send_resize`'s single post-boot SIGWINCH to grow into
        // the live area, a resize that races the agent's startup and, when
        // lost, leaves the pane pinned at ~50% width until live mode is
        // re-entered. See `Instance::ensure_pane_ready_with_size`.
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
        // Clear the resize dedup so `finalize_live_send_resize` always
        // issues its sync resize, even if the cached geometry from a
        // prior session happens to match the current preview_pane_area.
        self.live_send_last_resize = None;
        // Live mode takes over the pane's size from here; drop the non-live
        // preview dedup so exiting re-asserts the preview geometry cleanly.
        self.preview_pane_synced = None;
        self.stamp_last_accessed(session_id);
        Ok(())
    }

    /// Synchronously resize the live-send pane to match `self.preview_pane_area`,
    /// then block for ~50 ms so the agent has time to handle SIGWINCH and
    /// re-lay out before the next preview capture.
    ///
    /// Must be called after `prepare_live_send` returns `Ok(_)` and after
    /// the caller has redrawn the frame in the post-toast geometry the
    /// user will see for the next several frames. See `prepare_live_send`
    /// for why the two are split.
    ///
    /// `preview_pane_area` is the cached OUTPUT sub-rect: the full inner
    /// (after border + padding) minus the info header AND minus the
    /// inner ` Output ` / ` Terminal Output ` banner row when the user
    /// has the header expanded. Sizing to the full inner instead would
    /// leave the top `info_height + 1` rows of the agent's output
    /// outside the visible window; tail-clip semantics in the preview's
    /// `Paragraph` render then drop those rows on every frame, which
    /// the user perceives as content shifted up. The math is shared
    /// with the per-frame resize in `refresh_preview_cache_if_needed`
    /// and friends; the rect comes from
    /// `components::preview::PreviewLayout::compute`.
    pub fn finalize_live_send_resize(&mut self) {
        let Some(state) = self.live_send.as_ref() else {
            return;
        };
        let tmux_name = state.tmux_name.clone();
        let pane = self.preview_pane_area;
        if pane.width == 0 || pane.height == 0 {
            return;
        }
        // Size through `Session::resize_window` so the pane lands at exactly
        // `pane.height` after tmux's status-bar chrome (#2766), matching the
        // worker's Resize arm and the passive preview sync. A raw
        // `resize-window -y pane.height` leaves a `pane.height - chrome` pane
        // one row shorter than the preview output area, desyncing the live
        // preview by a row (#2742).
        let session = crate::tmux::Session::from_name(&tmux_name);
        // Only register the dedup if the session still exists (so the resize was
        // actually attempted). If it died between our state install and now,
        // leaving `live_send_last_resize` as None lets the next
        // `refresh_preview_cache_if_needed` retry through the worker.
        if session.exists() {
            session.resize_window(pane.width, pane.height);
            self.live_send_last_resize = Some((pane.width, pane.height));
        }
        // Give the agent ~50ms to handle SIGWINCH and re-lay out
        // before we capture the first frame. Some agents (claude-
        // code in particular) do a full clear-screen + redraw on
        // resize; capturing during that produces a partial frame.
        // 50ms is the smallest delay that empirically lets the
        // most-common agents settle.
        //
        // Wrap the sleep in `block_in_place` so the tokio
        // multi-threaded runtime can reschedule any other tasks
        // off this worker for the duration. Without it, the 50ms
        // would block every other tokio task (status pollers,
        // update checks, etc.) from running on this thread. The
        // call is a no-op on a current-thread runtime; aoe
        // always uses multi-threaded (`#[tokio::main]`).
        tokio::task::block_in_place(|| {
            std::thread::sleep(std::time::Duration::from_millis(50));
        });
    }
}
