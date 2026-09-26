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

    /// Auxiliary warmth follows its own observation, independently of agent status.
    fn target_pane_is_warm(&self, session_id: &str, target: &live_send::LiveSendTarget) -> bool {
        let Some(inst) = self.get_instance(session_id) else {
            return false;
        };
        use crate::session::{AuxiliaryTarget, PanePresence};
        let presence = match target {
            live_send::LiveSendTarget::Agent => return self.agent_pane_is_warm(session_id),
            live_send::LiveSendTarget::Terminal => {
                inst.auxiliary_presence(&AuxiliaryTarget::Host { index: 0 })
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                inst.auxiliary_presence(&AuxiliaryTarget::Container { index: 0 })
            }
            live_send::LiveSendTarget::Tool(name) => inst.tool_presence(name),
        };
        presence == PanePresence::Alive
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
    pub(in crate::tui) fn live_send_boot_size(&self) -> Option<(u16, u16)> {
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

    /// Take the target the next live-send entry should prepare.
    pub(in crate::tui) fn take_live_send_target(
        &mut self,
    ) -> crate::tui::home::live_send::LiveSendTarget {
        std::mem::replace(
            &mut self.pending_live_send_target,
            crate::tui::home::live_send::LiveSendTarget::Agent,
        )
    }

    /// Enter live send against a pane the daemon has already confirmed ready.
    /// `tmux_name` is the receipt's target, so nothing is resolved locally.
    pub(in crate::tui) fn enter_live_send_with(
        &mut self,
        session_id: &str,
        tmux_name: &str,
        target: crate::tui::home::live_send::LiveSendTarget,
        lease: crate::tui::session_feed::NativeLease,
    ) -> Result<(), ()> {
        if !lease.is_valid() || !self.session_feed.native_interaction_available() {
            return Err(());
        }
        let inst = match self.get_instance(session_id) {
            Some(inst) => inst.clone(),
            None => {
                self.info_dialog = Some(InfoDialog::new(
                    "Live send failed",
                    "Session disappeared before live mode could start.",
                ));
                return Err(());
            }
        };
        let tmux_name = tmux_name.to_string();
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
        self.live_send_worker = Some(live_send::LiveSendWorker::spawn(
            tmux_name,
            capture_wake,
            lease,
        ));
        // Start every live-mode entry (including a switch from another
        // session) with a disarmed leader menu, so a half-entered chord
        // can't carry over from a prior target.
        self.live_send_pending_leader = false;
        // The first post-toast draw queues the settled geometry through the
        // size-owning worker, even when a prior session used the same size.
        self.live_send_last_resize = None;
        self.live_send_resize_retry_at = None;
        // Live mode takes over the pane's size from here; drop the non-live
        // resize bookkeeping so exiting re-asserts the preview geometry
        // cleanly.
        self.clear_preview_pane_sync(session_id);
        self.stamp_last_accessed(session_id);
        Ok(())
    }
}
