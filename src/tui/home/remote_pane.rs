//! A selected remote session in the preview pane: its live output, its info
//! panel, and live-send into it, mirroring a local session's pane.

use ratatui::layout::{Alignment, Rect};
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use super::live_send;
use super::preview::PreviewCache;
use super::render::{capture_apply_step, capture_lines_for, clamp_scroll_to_capture, CaptureFit};
use super::HomeView;
use crate::session::RemoteShelf;
use crate::tui::app::Action;
use crate::tui::components::preview::{self, CachedPreview, Preview};
use crate::tui::remote_feed::shelf_of;
use crate::tui::remote_preview::{PreviewCommand, PreviewEvent, RemoteKey};
use crate::tui::styles::Theme;

/// A frame from the remote pane awaiting render, with the window it was
/// requested at so the shared capture rules can judge it.
pub(in crate::tui) struct RemoteFrame {
    pub(in crate::tui) content: String,
    pub(in crate::tui) cursor: crate::tmux::PaneCursor,
    pub(in crate::tui) budget: usize,
}

impl HomeView {
    fn remote_row_watchable(&self, key: &RemoteKey) -> bool {
        self.remote_instance(&key.0, &key.1)
            .is_some_and(|inst| shelf_of(inst) == RemoteShelf::Live && !inst.is_structured())
    }

    /// Point the preview socket at the selected remote row, or drop it. Cheap
    /// when nothing changed, so it runs on every selection sync and on the
    /// remote poll, which also retries a watch whose socket closed.
    pub(super) fn sync_remote_preview(&mut self) {
        let want = self
            .selected_remote
            .clone()
            .filter(|key| self.remote_row_watchable(key));
        if want == self.remote_preview_key {
            return;
        }
        if let Some(live) = self.remote_live_key() {
            if want.as_ref() != Some(&live) {
                self.clear_live_send_state();
            }
        }
        self.remote_preview_cache = PreviewCache::default();
        self.remote_preview_frame = None;
        self.remote_preview_key = None;
        let Some(key) = want else {
            self.remote_preview_error = None;
            self.remote_preview.send(PreviewCommand::Stop);
            return;
        };
        match crate::tui::remote_feed::remote_endpoint(&key.0) {
            Some(endpoint) => {
                self.remote_preview_error = None;
                let lines = capture_lines_for(self.preview_visible_rows as u16, 0);
                self.remote_preview.send(PreviewCommand::Watch {
                    key: key.clone(),
                    endpoint,
                    lines,
                });
                self.remote_window_sent = Some((lines, true));
                self.remote_preview_key = Some(key);
            }
            None => {
                self.remote_preview_error = Some(format!("remote {:?} is not configured", key.0));
                self.remote_preview.send(PreviewCommand::Stop);
            }
        }
    }

    /// Land socket state and the newest frame from the preview worker.
    /// Returns whether the pane needs a redraw.
    pub fn apply_remote_preview(&mut self) -> bool {
        let mut changed = false;
        while let Some(event) = self.remote_preview.try_recv() {
            let current = self.remote_preview_key.clone();
            match event {
                PreviewEvent::SizeOwner {
                    key,
                    is_owner,
                    holder,
                } if self.remote_live_key().as_ref() == Some(&key) => {
                    if is_owner {
                        self.remote_live_granted = true;
                        continue;
                    }
                    // Refused, or another client took the pane; local
                    // live-send yields the same way rather than fighting. A
                    // take-over gets the notice, not a flash: it swallows the
                    // keys already typed for the pane so none of them lands on
                    // the session list as a shortcut.
                    let title = self.live_send.as_ref().map_or("", |l| l.title.as_str());
                    let taken = self.remote_live_granted;
                    let taker = holder.unwrap_or_else(|| "another client".to_string());
                    let message = if taken {
                        format!("Live mode ended: {taker} took over {title} on {}.", key.0)
                    } else {
                        format!("{} did not grant input to {title}", key.0)
                    };
                    self.exit_live_send_if_active();
                    if taken {
                        self.info_dialog = Some(crate::tui::dialogs::InfoDialog::new(
                            "Live send ended",
                            &message,
                        ));
                    } else {
                        self.flash_status(message);
                    }
                    changed = true;
                }
                PreviewEvent::Closed { key, reason } if current.as_ref() == Some(&key) => {
                    if self.remote_live_key().as_ref() == Some(&key) {
                        if let Some(live) = self.live_send.clone() {
                            self.clear_live_send_state();
                            self.flash_status(format!(
                                "Live input to {} ended: {reason}",
                                live.title
                            ));
                        }
                    }
                    self.remote_preview_error = Some(reason);
                    // Cleared so the next remote poll retries the watch.
                    self.remote_preview_key = None;
                    changed = true;
                }
                _ => {}
            }
        }
        if let Some(frame) = self.remote_preview.take_frame() {
            if self.remote_preview_key.as_ref() == Some(&frame.key) {
                self.remote_preview_frame = Some(RemoteFrame {
                    content: frame.content,
                    cursor: frame.cursor,
                    budget: self.remote_window_sent.map_or(0, |(lines, _)| lines),
                });
                self.remote_preview_error = None;
                changed = true;
            }
        }
        changed
    }

    pub(super) fn remote_live_key(&self) -> Option<RemoteKey> {
        self.live_send.as_ref()?.remote_key()
    }

    /// Enter live-send on the selected remote row: claim the pane's size and
    /// route keys to it. Rows that cannot take input explain why instead.
    pub(super) fn start_remote_live_send(&mut self) -> Option<Action> {
        let (remote, id) = self.selected_remote.clone()?;
        let row = self.remote_instance(&remote, &id)?;
        if row.is_structured() {
            return Some(Action::SetTransientStatus(
                "A structured session has no terminal to live-send into; press Enter to open it"
                    .to_string(),
            ));
        }
        match shelf_of(row) {
            RemoteShelf::Archived => {
                return Some(Action::SetTransientStatus(format!(
                    "Archived on {remote}; restore it there to open it"
                )));
            }
            RemoteShelf::Trashed => {
                return Some(Action::SetTransientStatus(format!(
                    "In {remote}'s trash; restore it there to open it"
                )));
            }
            RemoteShelf::Live => {}
        }
        let title = row.title.clone();
        let key = (remote.clone(), id.clone());
        if self.remote_live_key().as_ref() == Some(&key) {
            return None;
        }
        self.exit_live_send_if_active();
        self.sync_remote_preview();
        if self.remote_preview_key.as_ref() != Some(&key) {
            return Some(Action::SetTransientStatus(format!("Can't reach {remote}")));
        }
        let (cols, rows) = (
            self.preview_pane_area.width.max(1),
            self.preview_pane_area.height.max(1),
        );
        // Deliberate policy: live-send owns the pane's size, as local
        // live-send does.
        self.remote_preview
            .send(PreviewCommand::TakeOver { cols, rows });
        self.remote_live_size = (cols, rows);
        self.remote_live_granted = false;
        self.live_send_pending_leader = false;
        self.live_send = Some(live_send::LiveSendState::new(
            id,
            title,
            String::new(),
            live_send::LiveSendTarget::Agent,
            Some(remote),
            &crate::session::config::profile_config::resolve_config_or_warn(&self.config_profile())
                .session,
        ));
        None
    }

    /// Reconnect the watch after remote live-send, which releases the
    /// size-owner lock the live-send took.
    pub(super) fn release_remote_pane(&mut self) {
        self.remote_live_granted = false;
        self.remote_preview_key = None;
        self.sync_remote_preview();
    }

    /// Send a live-send key to the remote pane; the worker holds it until the
    /// daemon grants input.
    pub(super) fn send_remote_input(&self, key: &live_send::TmuxKey) {
        let bytes = live_send::encode_key_bytes(key, false);
        if !bytes.is_empty() {
            self.remote_preview.send(PreviewCommand::Input(bytes));
        }
    }

    /// Scroll a watched remote pane this viewer is not driving. `col`/`row` are
    /// 0-based pane cells; the daemon encodes the notch for the pane's modes.
    pub(super) fn send_remote_wheel(&self, up: bool, col: u16, row: u16) {
        self.remote_preview.wheel(up, col, row);
    }

    /// Render the selected remote row into the preview pane.
    pub(super) fn render_remote_preview(
        &mut self,
        frame: &mut Frame,
        inner: Rect,
        theme: &Theme,
        compact: bool,
    ) {
        let Some((remote, id)) = self.selected_remote.clone() else {
            return;
        };
        let Some(inst) = self.remote_instance(&remote, &id).cloned() else {
            return;
        };
        let layout = preview::PreviewLayout::compute(
            inner,
            compact,
            self.show_preview_info,
            preview::agent_info_height(&inst),
        );
        self.preview_pane_area = layout.output;
        self.preview_visible_rows = layout.output.height as usize;

        if self.remote_live_key().is_some() {
            let size = (layout.output.width.max(1), layout.output.height.max(1));
            if size != self.remote_live_size {
                self.remote_live_size = size;
                self.remote_preview.resize(size.0, size.1);
            }
        }

        let hint = self
            .remote_preview_error
            .as_ref()
            .filter(|_| self.remote_preview_cache.is_pending_for(&id))
            .map(|e| format!("{remote}: {e}"));
        if let Some(hint) = hint {
            if let Some(info) = layout.info {
                Preview::render_info(frame, info, &inst, theme, self.idle_decay_window);
            }
            frame.render_widget(
                Paragraph::new(hint)
                    .style(Style::default().fg(theme.dimmed))
                    .alignment(Alignment::Center),
                layout.output,
            );
            return;
        }

        self.apply_remote_frame(&id, layout.output);
        self.remote_preview_cache.ensure_parsed();
        let line_count = self
            .remote_preview_cache
            .parsed_text
            .as_ref()
            .map_or(0, |t| t.lines.len());
        self.set_preview_text_view(layout.output, line_count);
        Preview::render_with_cache(
            frame,
            inner,
            &inst,
            CachedPreview::new(
                self.remote_preview_cache.parsed_text.as_ref(),
                self.remote_preview_cache.is_pending_for(&id),
            ),
            self.preview_scroll_offset,
            theme,
            self.idle_decay_window,
            compact,
            self.show_preview_info,
        );
    }

    /// Ask the socket for the window the read needs and apply the newest
    /// frame under the local capture rules: hold it while reading scrollback
    /// unless it extends what the held snapshot covers.
    fn apply_remote_frame(&mut self, id: &str, output: Rect) {
        let scroll_offset = self.preview_scroll_offset;
        let window = (
            remote_window_lines(&self.remote_preview_cache, id, output.height, scroll_offset),
            scroll_offset == 0,
        );
        if self.remote_window_sent != Some(window) {
            self.remote_preview.send(PreviewCommand::Window {
                lines: window.0,
                fast: window.1,
            });
            self.remote_window_sent = Some(window);
        }
        let Some(frame) = self.remote_preview_frame.take() else {
            return;
        };
        let cache = &self.remote_preview_cache;
        let held_covers = cache.session_id.as_deref() == Some(id)
            && self.preview_visible_rows + scroll_offset as usize <= cache.captured_lines;
        let Some(clamp) = capture_apply_step(CaptureFit {
            frozen: self.preview_is_frozen(),
            held_covers,
            incoming_lines: frame.content.lines().count(),
            empty: frame.content.is_empty(),
            budget: frame.budget,
            capture_lines: capture_lines_for(output.height, scroll_offset),
            height: output.height,
            scroll_offset,
        }) else {
            self.remote_preview_frame = Some(frame);
            return;
        };
        let captured_lines = self.remote_preview_cache.store_capture(
            frame.content,
            id.to_string(),
            String::new(),
            0,
            (output.width, output.height),
            Some(frame.cursor),
        );
        if clamp {
            self.preview_scroll_offset =
                clamp_scroll_to_capture(scroll_offset, captured_lines, self.preview_visible_rows);
        }
    }
}

/// Lines to ask the remote to capture. Reading scrollback wants the whole
/// history once; after a snapshot holding all of it is in, every further wide
/// frame would only be held, so the socket drops back to a screen-sized window
/// instead of shipping thousands of lines per publish until the read ends.
fn remote_window_lines(cache: &PreviewCache, id: &str, height: u16, scroll_offset: u16) -> usize {
    let wanted = capture_lines_for(height, scroll_offset);
    let complete = scroll_offset > 0
        && !cache.is_pending_for(id)
        && cache.cursor.is_some_and(|c| {
            let pane_lines = c.history_size as usize + c.pane_height as usize;
            cache.captured_lines >= wanted.min(pane_lines)
        });
    if complete {
        capture_lines_for(height, 0)
    } else {
        wanted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_asks_for_the_history_until_a_snapshot_holds_all_of_it() {
        let live = capture_lines_for(40, 0);
        let reading = capture_lines_for(40, 30);
        let cache = |id: &str, captured_lines: usize, history_size: u32| {
            let mut cache = PreviewCache {
                session_id: Some(id.to_string()),
                captured_lines,
                ..Default::default()
            };
            cache.cursor = Some(crate::tmux::PaneCursor {
                x: 0,
                y: 0,
                visible: true,
                pane_height: 40,
                history_size,
                pane_width: 80,
                alternate_on: false,
                mouse_tracking: false,
                mouse_sgr: false,
                mouse_all: false,
                position_reliable: true,
                composite_pane0: None,
            });
            cache
        };
        let cases = [
            ("at the live edge", cache("r1", 60, 500), 0, live),
            ("live capture only", cache("r1", 60, 500), 30, reading),
            ("another row's snapshot", cache("r2", 540, 500), 30, reading),
            ("all history held", cache("r1", 540, 500), 30, live),
            (
                "the budget's worth held",
                cache("r1", reading, 9000),
                30,
                live,
            ),
            (
                "reading past the budget",
                cache("r1", reading, 9000),
                3000,
                capture_lines_for(40, 3000),
            ),
        ];
        for (what, cache, offset, want) in cases {
            assert_eq!(
                remote_window_lines(&cache, "r1", 40, offset),
                want,
                "{what}"
            );
        }
    }
}
