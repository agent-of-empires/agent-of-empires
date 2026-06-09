//! Input handling for HomeView

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::Position;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::bindings::{self, ActionId};
use super::{live_send, DragKind, HomeView, PreviewSelection, TerminalMode, ViewMode};
use crate::session::config::{load_config, save_config, GroupByMode, SortOrder};
use crate::session::{list_profiles, repo_config, resolve_config_or_warn, Item, Status};
use crate::tui::app::Action;
#[cfg(feature = "serve")]
use crate::tui::dialogs::ServeAction;
use crate::tui::dialogs::{
    builtin_commands, CommandPaletteDialog, ConfirmDialog, ContextMenuAction, ContextMenuDialog,
    DeleteDialogConfig, DialogResult, GroupDeleteOptionsDialog, HooksInstallDialog, InfoDialog,
    IntroOutcome, NewSessionData, NewSessionDialog, NoAgentsAction, PaletteAction, PaletteCommand,
    PaletteGroup, ProfilePickerAction, ProjectsDialog, RenameDialog, RenameMode, RepoTrustAction,
    RestartDialog, SendMessageDialog, UnifiedDeleteDialog, WorktreeNameDialog,
};
use crate::tui::diff::{DiffAction, DiffView};
use crate::tui::responsive;
use crate::tui::settings::{SettingsAction, SettingsView};

/// Maximum gap between two left-clicks on the same row that still
/// counts as a double-click. 400ms matches the default on most desktop
/// environments. Worth tuning if real-world feedback says it's too
/// fast for trackpads or too slow on remote sessions.
const DOUBLE_CLICK_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(400);

/// Persist the user's picks from the first-run intro wizard. Theme name goes
/// to `config.theme.name`; attach mode is mirrored to both
/// `new_session_attach_mode` (post-create) and `default_attach_mode`
/// (Enter/double-click) so the two paths stay consistent. Failures are
/// logged and swallowed: the intro should never block startup on a config
/// write hiccup.
fn apply_intro_outcome(outcome: &IntroOutcome) {
    if outcome.final_theme.is_none()
        && outcome.final_attach_mode.is_none()
        && outcome.telemetry_opt_in.is_none()
    {
        return;
    }
    let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) else {
        tracing::warn!(target: "tui.input", "intro outcome: load_config failed; not persisting");
        return;
    };
    if let Some(theme) = &outcome.final_theme {
        config.theme.name = theme.clone();
    }
    if let Some(mode) = outcome.final_attach_mode {
        config.session.new_session_attach_mode = mode;
        config.session.default_attach_mode = mode;
    }
    if let Some(opt_in) = outcome.telemetry_opt_in {
        config.telemetry.enabled = opt_in;
        config.app_state.has_responded_to_telemetry = true;
    }
    if let Err(e) = save_config(&config) {
        tracing::warn!(target: "tui.input", "Failed to persist intro outcome: {e}");
    }
    // Sync the install id with the saved opt-in choice (no-op under
    // DO_NOT_TRACK). Done after save so telemetry.json matches config.
    if let Some(opt_in) = outcome.telemetry_opt_in {
        crate::telemetry::apply_opt_in_change(opt_in);
    }
}

/// Persist the user's answer to the standalone telemetry consent popup.
/// Sets the opt-in flag, marks the prompt answered so it never re-appears,
/// and reconciles the install id (no-op under `DO_NOT_TRACK`).
fn persist_telemetry_consent(opt_in: bool) {
    let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) else {
        tracing::warn!(target: "tui.input", "telemetry consent: load_config failed; not persisting");
        return;
    };
    config.telemetry.enabled = opt_in;
    config.app_state.has_responded_to_telemetry = true;
    if let Err(e) = save_config(&config) {
        tracing::warn!(target: "tui.input", "Failed to persist telemetry consent: {e}");
    }
    crate::telemetry::apply_opt_in_change(opt_in);
}

/// xterm bracketed-paste start sequence: `ESC [ 2 0 0 ~`. An agent that
/// has enabled bracketed paste mode (`\e[?2004h`) treats everything
/// between this marker and the matching end marker as one paste rather
/// than as keystrokes, so interior newlines accumulate in the input
/// buffer instead of firing `submit` per line.
const BRACKETED_PASTE_START: &[u8] = &[0x1b, b'[', b'2', b'0', b'0', b'~'];

/// xterm bracketed-paste end sequence: `ESC [ 2 0 1 ~`. Pairs with
/// [`BRACKETED_PASTE_START`].
const BRACKETED_PASTE_END: &[u8] = &[0x1b, b'[', b'2', b'0', b'1', b'~'];

/// Decompose pasted text into a series of `TmuxKey`s safe for the
/// live-send worker to dispatch.
///
/// Single-line pastes (no `\n` / `\r`) skip the bracketed-paste
/// wrapping and travel as a single `Literal`: a bare shell or any
/// agent that hasn't enabled `\e[?2004h` keeps working unchanged,
/// because we never emit the escape markers it would render as
/// literal text. Tabs in single-line pastes still go through as
/// `Named("Tab")` to mirror the historical path.
///
/// Multi-line pastes get wrapped in xterm bracketed-paste markers
/// (`\e[200~` / `\e[201~`) so the receiving agent sees the entire
/// payload as one paste rather than as N independent Enter keypresses.
/// Without the wrapping, agents that submit on Enter (Claude Code,
/// Codex, OpenCode, ...) post one user message per pasted line, which
/// is the bug behind #1546. The whole payload (markers, printable
/// runs, interior CRs, and tabs) goes through as a single `HexBytes`
/// action, which the worker dispatches as one or more size-bounded
/// `tmux send-keys -H` forks (a per-byte argv overflows `ARG_MAX` on a
/// large paste, so it can't always be one fork). `\r\n` pairs coalesce
/// to a single CR so
/// Windows-line-ending pastes don't double up; other control bytes
/// (BEL, ESC, ...) are dropped rather than risk that an embedded
/// escape closes the bracketed-paste sequence on the agent's side.
pub(super) fn split_paste_for_live_send(text: &str) -> Vec<live_send::TmuxKey> {
    let has_newline = text.contains('\n') || text.contains('\r');
    if !has_newline {
        return split_inline_paste(text);
    }
    split_bracketed_paste(text)
}

fn split_inline_paste(text: &str) -> Vec<live_send::TmuxKey> {
    let mut out = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        let is_control = (ch as u32) < 0x20 || ch == '\x7f';
        if !is_control {
            buf.push(ch);
            continue;
        }
        if !buf.is_empty() {
            out.push(live_send::TmuxKey::Literal(std::mem::take(&mut buf)));
        }
        if ch == '\t' {
            out.push(live_send::TmuxKey::Named("Tab".to_string()));
        }
        // BEL, ESC, etc.: dropping is friendlier than mapping
        // to a named key that could cancel the agent's input.
    }
    if !buf.is_empty() {
        out.push(live_send::TmuxKey::Literal(buf));
    }
    out
}

fn split_bracketed_paste(text: &str) -> Vec<live_send::TmuxKey> {
    // Build one contiguous byte payload: start marker, then the paste
    // content with printables as their UTF-8 bytes / interior newlines
    // as CR (0x0d) / tabs as 0x09, then the end marker. Sending it as
    // one `HexBytes` means the worker fires exactly one `tmux send-keys
    // -H` subprocess per paste rather than one per chunk.
    let mut bytes = Vec::with_capacity(text.len() + BRACKETED_PASTE_START.len() * 2);
    bytes.extend_from_slice(BRACKETED_PASTE_START);

    let mut chars = text.chars().peekable();
    let mut utf8_buf = [0u8; 4];
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => bytes.push(0x0d),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                bytes.push(0x0d);
            }
            '\t' => bytes.push(0x09),
            c if (c as u32) < 0x20 || c == '\x7f' => {
                // Embedded ESC / BEL / etc. has no safe encoding
                // inside the paste payload; drop rather than risk a
                // bogus terminal escape closing the paste early.
            }
            c => {
                let s = c.encode_utf8(&mut utf8_buf);
                bytes.extend_from_slice(s.as_bytes());
            }
        }
    }

    bytes.extend_from_slice(BRACKETED_PASTE_END);
    vec![live_send::TmuxKey::HexBytes(bytes)]
}

fn resolve_hook_install_agent(
    tool_name: &str,
    session_config: &crate::session::config::SessionConfig,
) -> Option<&'static crate::agents::AgentDef> {
    crate::agents::get_agent(tool_name)
        .or_else(|| {
            session_config
                .agent_detect_as
                .get(tool_name)
                .and_then(|detect_as| crate::agents::get_agent(detect_as))
        })
        .filter(|agent| agent.hook_config.is_some())
}

pub(super) fn parse_hotkey(s: &str) -> Option<(KeyCode, KeyModifiers)> {
    let (modifier, key) = s.split_once('+')?;
    if !modifier.eq_ignore_ascii_case("alt") {
        return None;
    }
    let mut chars = key.chars();
    let ch = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some((KeyCode::Char(ch.to_ascii_lowercase()), KeyModifiers::ALT))
}

/// Validate tool hotkey strings, returning a list of human-readable warning
/// lines for any that fail to parse. Tool hotkeys must match the
/// `Alt+<single-char>` shape; everything else is rejected so the user gets
/// a clear error rather than a silently dead binding.
pub(super) fn validate_tool_hotkeys(
    tools: &std::collections::HashMap<String, crate::session::config::ToolSessionConfig>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, config) in tools {
        if let Some(ref hotkey) = config.hotkey {
            if parse_hotkey(hotkey).is_none() {
                let msg = format!(
                    "Tool '{}': invalid hotkey '{}' (expected format: Alt+<letter>)",
                    name, hotkey
                );
                tracing::warn!("{}", msg);
                warnings.push(msg);
            }
        }
    }
    warnings
}

/// Build a sorted lookup list of `(tool_name, KeyCode, KeyModifiers)` for
/// every tool whose `hotkey` parses. Sorted by tool name so the
/// alphabetically-first tool wins on duplicate hotkeys. Built once at
/// startup and on settings reload, then iterated on every keystroke;
/// keep it cheap.
pub(super) fn build_tool_hotkey_cache(
    tools: &std::collections::HashMap<String, crate::session::config::ToolSessionConfig>,
) -> Vec<(String, KeyCode, KeyModifiers)> {
    let mut sorted: Vec<_> = tools.iter().collect();
    sorted.sort_by_key(|(name, _)| name.to_owned());
    sorted
        .into_iter()
        .filter_map(|(name, config)| {
            let hotkey_str = config.hotkey.as_deref()?;
            let (code, modifiers) = parse_hotkey(hotkey_str)?;
            Some((name.clone(), code, modifiers))
        })
        .collect()
}

/// Pull the cell symbols in columns `[from, to_excl)` out of a parsed
/// scrollback `Line`. The line is laid back out into a one-row buffer at
/// the pane width so wide characters, combining marks, and right-edge
/// truncation resolve exactly as ratatui rendered them on screen; reading
/// cell symbols then mirrors the old frame-buffer extraction cell for
/// cell. Unwritten cells read as a single space, which the caller trims.
fn slice_line_columns(line: &ratatui::text::Line, from: u16, to_excl: u16, width: u16) -> String {
    let mut buf = ratatui::buffer::Buffer::empty(ratatui::layout::Rect::new(0, 0, width, 1));
    buf.set_line(0, 0, line, width);
    let hi = to_excl.min(width);
    let mut out = String::new();
    for col in from..hi {
        out.push_str(buf[(col, 0)].symbol());
    }
    out
}

impl HomeView {
    pub fn is_diff_open(&self) -> bool {
        self.diff_view.is_some()
    }

    pub fn hit_preview(&self, col: u16, row: u16) -> bool {
        self.preview_area.contains(Position::from((col, row)))
    }

    /// Drain a theme name queued by intro-dialog clicks. The mouse handler in
    /// `App` calls this after `handle_dialog_click` and dispatches
    /// `Action::SetTheme` so the live preview / final pick applies through
    /// the same path the keyboard route uses.
    pub fn take_pending_intro_theme(&mut self) -> Option<String> {
        self.pending_intro_theme.take()
    }

    pub fn hit_diff(&self, col: u16, row: u16) -> bool {
        self.diff_area.contains(Position::from((col, row)))
    }

    /// Forward a left-click to the diff view's file-list panel. No-op
    /// when no diff view is open.
    pub fn handle_diff_click(&mut self, col: u16, row: u16) {
        if let Some(view) = &mut self.diff_view {
            view.handle_click(col, row);
        }
    }

    /// Forward a hover event to the diff view's file-list panel.
    /// Returns true when the focused file changed.
    pub fn handle_diff_hover(&mut self, col: u16, row: u16) -> bool {
        if let Some(view) = &mut self.diff_view {
            view.handle_hover(col, row)
        } else {
            false
        }
    }

    fn open_tool_picker(&mut self) {
        self.tool_picker_dialog = Some(crate::tui::dialogs::ToolPickerDialog::new(
            &self.tool_configs,
        ));
    }

    /// Check if the key event matches any configured tool session hotkey.
    /// On duplicate hotkeys, the alphabetically-first tool name wins
    /// (the cache is built sorted by tool name).
    fn match_tool_hotkey(&self, key: &KeyEvent) -> Option<String> {
        for (name, code, modifiers) in &self.tool_hotkey_cache {
            if key.code == *code && key.modifiers == *modifiers {
                return Some(name.clone());
            }
        }
        None
    }

    pub fn hit_list(&self, col: u16, row: u16) -> bool {
        self.list_area.contains(Position::from((col, row)))
    }

    /// True when `(col, row)` lands on the side-by-side list/preview
    /// divider. The divider is the preview's left border column (one
    /// past `list_area.right()` is exclusive, so this *is* a valid hit
    /// target that hit_list / hit_preview both miss by design). Returns
    /// `false` in stacked mode, in the diff/settings/serve takeover
    /// views (which clear `divider_col`), and while any modal dialog is
    /// open, so a dialog over the divider swallows stray clicks rather
    /// than starting a hidden drag.
    pub fn hit_divider(&self, col: u16, row: u16) -> bool {
        if self.has_dialog() {
            return false;
        }
        let Some(div_col) = self.divider_col else {
            return false;
        };
        if col != div_col {
            return false;
        }
        let list_y = self.list_area.y;
        let list_bottom = self.list_area.bottom();
        row >= list_y && row < list_bottom
    }

    /// Begin a drag if `(col, row)` is on the divider, or inside the
    /// preview pane (whenever the pane is on screen, in or out of live
    /// mode). Returns true when a drag actually started, so the caller
    /// can mark the event handled and skip the row-click path.
    ///
    /// Divider drags resize the list/preview split (the only kind we
    /// had before live-send shipped). Preview-pane drags start an
    /// in-app text selection: terminal-native drag-select can't reach
    /// the preview because we capture mouse events to support wheel
    /// scroll, and we want one mechanism that also works on Mosh and
    /// mobile clients where Shift-bypass does nothing.
    pub fn handle_drag_start(&mut self, col: u16, row: u16) -> bool {
        if self.hit_divider(col, row) {
            self.drag_state = Some(DragKind::ListDivider {
                start_col: col,
                start_width: self.list_width,
            });
            return true;
        }
        // Modals that aren't live-send sit over the preview, so a
        // click inside `preview_area` while one is open is meant for
        // the modal underneath and should not seed a hidden selection
        // behind it. `handle_drag_move`'s cancel branch covers a modal
        // that opens mid-drag; this guards the start.
        if self.has_non_live_send_overlay() {
            return false;
        }
        // Seed the selection in content coords so it survives a scroll
        // and can span more than one page. `contains` also requires the
        // pane to hold real scrollback, so a drag over an empty / not-yet
        // -captured pane is a no-op rather than a phantom selection.
        let view = self.preview_text_view;
        if view.contains(col, row) {
            let cell = view.screen_to_content(col, row);
            self.preview_selection = Some(PreviewSelection {
                anchor: cell,
                extent: cell,
                finalized: false,
            });
            self.drag_state = Some(DragKind::PreviewSelect);
            return true;
        }
        false
    }

    /// Apply a drag-in-progress event. For the list divider, recompute
    /// the requested width from `(start_width + delta)` and clamp to
    /// `[10, main_area_width - PREVIEW_MIN_WIDTH]` so the preview keeps
    /// its usability floor and the value never wraps `u16`. For a
    /// preview-pane text selection, clamp the extent to the preview
    /// area and stash it on `preview_selection`; the renderer reads it
    /// each frame to paint the highlight.
    ///
    /// Returns true when state actually changed (so the caller
    /// redraws). The drag does NOT persist on every tick; the divider
    /// path saves on release, the preview-select path emits OSC 52 on
    /// release.
    pub fn handle_drag_move(&mut self, col: u16, row: u16) -> bool {
        // A dialog opened mid-drag (e.g. a hotkey pressed while the
        // mouse button is still held) shouldn't keep updating the
        // sidebar invisibly under the modal. End the drag here so the
        // next `Up(Left)` is a no-op, persisting whatever width the
        // user dragged to before the dialog covered it (handle_drag_end
        // is the normal save site, so we mirror its behavior).
        //
        // `has_dialog()` returns true while live-send is active, which
        // is exactly when preview drag-select is meant to work. Live
        // mode is therefore exempt; but a real modal (info / confirm
        // / palette / picker) opening mid-select must also kill the
        // drag and drop the selection so it can't keep mutating under
        // the modal or finalize on mouse-up behind the overlay.
        let drag_is_preview = matches!(self.drag_state, Some(DragKind::PreviewSelect));
        let cancel_drag = if drag_is_preview {
            self.has_non_live_send_overlay()
        } else {
            self.has_dialog()
        };
        if cancel_drag && self.drag_state.is_some() {
            self.drag_state = None;
            if drag_is_preview {
                self.preview_selection = None;
                self.preview_copy_pending = false;
                self.preview_copy_text = None;
            } else {
                self.save_list_width();
            }
            return false;
        }
        match self.drag_state {
            Some(DragKind::ListDivider {
                start_col,
                start_width,
            }) => {
                // i32 arithmetic so a leftward drag past the start column doesn't
                // underflow u16 before the clamp.
                let delta = col as i32 - start_col as i32;
                let proposed = start_width as i32 + delta;

                // Clamp ceiling tracks the live viewport width; if the user
                // resized the terminal mid-drag, the new width is honored. The
                // floor of 10 matches the keyboard `<` shrink limit.
                let ceiling = self
                    .main_area_width
                    .saturating_sub(responsive::PREVIEW_MIN_WIDTH);
                let max_width = ceiling.max(10);
                let clamped = proposed.clamp(10, max_width as i32) as u16;

                if clamped == self.list_width {
                    return false;
                }
                self.list_width = clamped;
                true
            }
            Some(DragKind::PreviewSelect) => {
                let view = self.preview_text_view;
                let pane = view.pane;
                if view.total_lines == 0 || pane.width == 0 || pane.height == 0 {
                    return false;
                }
                // Record the live pointer cell so the ticker-driven
                // auto-scroll (`tick_preview_autoscroll`) can keep
                // extending while the cursor is held at the edge. The
                // scroll itself is NOT done here: crossterm only emits
                // Drag events on movement, so scrolling per-event makes
                // a held cursor stall and a moving one lurch one line per
                // event. The ticker advances it smoothly instead.
                self.preview_drag_pos = Some((col, row));
                let new_extent = view.screen_to_content(col, row);
                let Some(sel) = self.preview_selection.as_mut() else {
                    return false;
                };
                if sel.extent == new_extent {
                    return false;
                }
                sel.extent = new_extent;
                true
            }
            None => false,
        }
    }

    /// Advance an edge-held preview drag by one line. Driven by the event
    /// loop's ~33ms ticker (not by mouse events) so holding the cursor at
    /// the pane's top or bottom edge scrolls continuously and grows the
    /// selection past a single page. Returns whether anything moved, so
    /// the caller can redraw.
    pub fn tick_preview_autoscroll(&mut self) -> bool {
        if !matches!(self.drag_state, Some(DragKind::PreviewSelect)) {
            return false;
        }
        let Some((col, row)) = self.preview_drag_pos else {
            return false;
        };
        let view = self.preview_text_view;
        let pane = view.pane;
        if view.total_lines == 0 || pane.width == 0 || pane.height == 0 {
            return false;
        }
        let at_top = row <= pane.y;
        let at_bottom = row >= pane.bottom().saturating_sub(1);
        if !at_top && !at_bottom {
            // Cursor pulled back inside the pane: arm the next edge entry
            // to scroll immediately rather than wait out the interval.
            self.preview_autoscroll_at = None;
            return false;
        }
        // Pace the scroll to a steady cadence regardless of how often the
        // loop woke this iteration, so the speed is even instead of racing
        // with capture-worker activity.
        const AUTOSCROLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);
        let now = std::time::Instant::now();
        if let Some(prev) = self.preview_autoscroll_at {
            if now.duration_since(prev) < AUTOSCROLL_INTERVAL {
                return false;
            }
        }
        let scrolled = if at_top {
            self.scroll_preview_offset(1)
        } else {
            self.scroll_preview_offset(-1)
        };
        if !scrolled {
            return false;
        }
        self.preview_autoscroll_at = Some(now);
        let col_off = col.clamp(pane.x, pane.right().saturating_sub(1)) - pane.x;
        // Pin the extent to the now-revealed edge line in `from_bottom`
        // terms, which the new scroll offset gives directly: the bottom
        // visible line sits `offset` lines up from the newest line, the top
        // visible line `offset + height - 1`. Deriving it from the offset
        // (not the stale pre-scroll `total_lines`) keeps it correct even
        // before the next frame re-captures.
        let offset = self.preview_scroll_offset as usize;
        let from_bottom = if at_top {
            offset + (pane.height as usize).saturating_sub(1)
        } else {
            offset
        };
        if let Some(sel) = self.preview_selection.as_mut() {
            sel.extent = (col_off, from_bottom);
        }
        true
    }

    /// Shift the preview scroll offset by `delta` lines (positive scrolls
    /// up toward older output), clamped to the captured window. Returns
    /// whether the offset actually moved. Factored out of the wheel
    /// handlers so the edge auto-scroll can move the pane without dragging
    /// the whole `handle_scroll_*` routing along with it.
    fn scroll_preview_offset(&mut self, delta: i32) -> bool {
        let cache = self.active_preview_cache();
        let visible_height = cache.dimensions.1.saturating_sub(1) as usize;
        let real_max = cache.captured_lines.saturating_sub(visible_height) as i32;
        let new = (self.preview_scroll_offset as i32 + delta).clamp(0, real_max) as u16;
        if new == self.preview_scroll_offset {
            return false;
        }
        self.preview_scroll_offset = new;
        true
    }

    /// End any active drag. For the list divider, persist the final
    /// `list_width` to config so the new layout survives a restart.
    /// For a preview-pane selection, mark it `finalized` so the
    /// renderer keeps the highlight visible until the user dismisses
    /// it. The actual clipboard copy is the caller's job (see
    /// `app.rs`) so we can keep this method side-effect-free besides
    /// state, which is what the existing divider path does too.
    ///
    /// Returns true when a drag was actually in progress, so the
    /// caller can avoid a spurious redraw on every `Up(Left)` that
    /// wasn't part of a drag.
    pub fn handle_drag_end(&mut self) -> bool {
        let Some(state) = self.drag_state.take() else {
            return false;
        };
        // The pointer is up, so edge auto-scroll stops; drop the tracked
        // position so a finalized highlight doesn't keep scrolling.
        self.preview_drag_pos = None;
        self.preview_autoscroll_at = None;
        match state {
            DragKind::ListDivider { .. } => {
                self.save_list_width();
            }
            DragKind::PreviewSelect => {
                // A bare click (no movement between Down and Up) collapses
                // anchor == extent. Treat that as "no selection" so a stray
                // click doesn't paint a 1x1 highlight or copy a single
                // character to the clipboard. Genuine multi-cell drags
                // get finalized so the renderer keeps the highlight visible
                // until dismissed; the next render also captures the
                // selected cells so the app loop can write them to the
                // user's clipboard once the buffer is drawn.
                if let Some(sel) = self.preview_selection {
                    if sel.anchor == sel.extent {
                        self.preview_selection = None;
                    } else if let Some(s) = self.preview_selection.as_mut() {
                        s.finalized = true;
                        self.preview_copy_pending = true;
                    }
                }
            }
        }
        true
    }

    /// Whether `drag_state` is currently a PreviewSelect (vs. a divider
    /// drag or nothing). Used by the Down(Left) handler in `app.rs` to
    /// tell whether `handle_drag_start` just installed a fresh selection
    /// or a divider drag.
    pub fn is_preview_select_dragging(&self) -> bool {
        matches!(self.drag_state, Some(DragKind::PreviewSelect))
    }

    /// Join the text under the current preview selection into a tmux-style
    /// flow string. Called from `paint_preview_selection` on the render
    /// that follows `handle_drag_end`, when `preview_copy_pending` is set.
    ///
    /// The selection is anchored to absolute scrollback lines, so this
    /// reads straight from the active cache's parsed `Text` rather than
    /// the visible frame buffer. That is what makes a multi-page copy work:
    /// the buffer only holds the current page, but the parsed cache holds
    /// the whole captured window, including the lines that have scrolled
    /// off screen. Each line is laid back out into a one-row buffer at the
    /// pane width so column slicing handles wide chars and truncation
    /// exactly as the on-screen render did.
    pub(super) fn extract_preview_selection_text(&self) -> Option<String> {
        let sel = self.preview_selection?;
        let view = self.preview_text_view;
        let width = view.pane.width;
        if width == 0 || view.total_lines == 0 {
            return None;
        }
        let lines = self.active_preview_cache().parsed_text.as_ref()?;
        // Resolve `from_bottom` distances to absolute indices against the
        // SAME `total_lines` the renderer used this frame, so the copied
        // range matches the painted highlight cell for cell.
        let ((start_col, start_line), (end_col, end_line)) = sel.ordered_abs(view);
        if start_line == end_line && start_col == end_col {
            return None;
        }
        let mut out = String::new();
        for line_idx in start_line..=end_line {
            let from = if line_idx == start_line { start_col } else { 0 };
            let to_excl = if line_idx == end_line {
                end_col.saturating_add(1).min(width)
            } else {
                width
            };
            if let Some(line) = lines.lines.get(line_idx) {
                if to_excl > from {
                    // Trim only trailing whitespace per row, not leading:
                    // a selection over indented code keeps the indentation,
                    // while right-edge padding on unfilled rows doesn't
                    // bloat the paste.
                    let slice = slice_line_columns(line, from, to_excl, width);
                    out.push_str(slice.trim_end());
                }
            }
            if line_idx < end_line {
                out.push('\n');
            }
        }
        if out.chars().all(char::is_whitespace) {
            return None;
        }
        Some(out)
    }

    /// Drain the text captured on the last render that painted a
    /// finalized preview selection. Returns `Some` exactly once per
    /// finalized drag; `App` calls this immediately after the draw
    /// to write the bytes to the clipboard.
    pub fn take_preview_copy_text(&mut self) -> Option<String> {
        self.preview_copy_text.take()
    }

    /// Discard any in-flight preview selection. Called from key/click
    /// paths so the user dismisses the highlight by interacting with
    /// the TUI again. Returns true when state actually changed (so the
    /// caller can redraw).
    pub fn clear_preview_selection(&mut self) -> bool {
        if self.preview_selection.take().is_some() {
            // Cancel any in-progress drag too so the next Up(Left)
            // doesn't re-finalize a stale selection, and stop the edge
            // auto-scroll from chasing a now-cleared selection.
            if matches!(self.drag_state, Some(DragKind::PreviewSelect)) {
                self.drag_state = None;
            }
            self.preview_drag_pos = None;
            self.preview_autoscroll_at = None;
            // A pending capture from a previous finalized drag is
            // moot once the selection is gone; drop it so the next
            // selection starts clean.
            self.preview_copy_pending = false;
            self.preview_copy_text = None;
            true
        } else {
            false
        }
    }

    /// Route a left-click into whichever modal dialog supports mouse
    /// input. Returns true when the click was consumed, in which case
    /// the caller must NOT fall through to the divider / preview / list
    /// handlers. Two cases both count as consumed: the dialog acted on
    /// the click (Yes/No), and the click landed elsewhere on screen
    /// while a modal is open (the modal absorbs it).
    ///
    /// Only the destructive delete dialog wires Yes/No clicks today;
    /// the existing modals continue to swallow mouse events implicitly
    /// via `has_dialog()` gates in the other handlers.
    /// Dispatch the Submit branch of a confirm dialog. Returns
    /// `Some(Action)` for confirm actions that emit a TUI action
    /// (`stop_session`, `quit_during_creation`); side-effect-only
    /// actions (`delete_group`, `force_remove_session`) run inline and
    /// return None. Shared by the keyboard Enter path and the
    /// mouse-click path so both flows produce the same end state.
    pub(super) fn dispatch_confirm_submit(&mut self, action: &str) -> Option<Action> {
        match action {
            "delete_group" => {
                if let Err(e) = self.delete_selected_group() {
                    tracing::error!(target: "tui.input", "Failed to delete group: {}", e);
                }
                None
            }
            "archive_group" => {
                if let Err(e) = self.archive_selected_group() {
                    tracing::error!(target: "tui.input", "Failed to archive group: {}", e);
                }
                None
            }
            "stop_session" => self.pending_stop_session.take().map(Action::StopSession),
            "force_remove_session" => {
                if let Some(session_id) = self.pending_force_remove_session.take() {
                    if let Err(e) = self.force_remove_session(&session_id) {
                        tracing::error!(target: "tui.input", "Failed to force remove session: {}", e);
                    }
                }
                None
            }
            "quit_during_creation" => Some(Action::Quit),
            "quit" => Some(Action::Quit),
            _ => None,
        }
    }

    /// Confirm before archiving every active session under the focused group.
    /// Archiving a whole project at once is a bigger hammer than the single-row
    /// `z`, so it routes through a prompt. Archiving is reversible, hence the
    /// calmer neutral tone rather than the destructive red. No-ops silently
    /// (no prompt) when the group has no active sessions left to archive.
    pub(super) fn prompt_archive_selected_group(&mut self) {
        let Some(group_path) = self.selected_group.clone() else {
            return;
        };
        let count = self.active_sessions_in_selected_group().len();
        if count == 0 {
            return;
        }
        // Project mode groups by repo, Manual mode by user-assigned path; name
        // the scope accordingly and show the full path so nested groups that
        // share a leaf segment aren't ambiguous.
        let (title, scope) = if self.group_by == crate::session::config::GroupByMode::Project {
            ("Archive project", "project")
        } else {
            ("Archive group", "group")
        };
        let noun = if count == 1 { "session" } else { "sessions" };
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                title,
                &format!("Archive all {count} {noun} in {scope} \"{group_path}\"?"),
                "archive_group",
            )
            .neutral(),
        );
    }

    /// Discard unsaved Settings changes: force-close the view, clear the
    /// confirm state, and revert any live theme preview back to the saved
    /// config theme. Shared by the keyboard (Esc/q -> confirm) and mouse
    /// (click [Yes]) discard paths so the two can't drift. The mouse path
    /// previously skipped the theme revert, so discarding via a click left a
    /// previewed theme applied until the next restart.
    pub(super) fn discard_settings_changes(&mut self) -> Action {
        if let Some(ref mut settings) = self.settings_view {
            settings.force_close();
        }
        self.settings_view = None;
        self.confirm_dialog = None;
        self.settings_close_confirm = false;
        // Theme is a global preference, not profile-merged: revert any live
        // preview to the saved global theme so boot and Settings agree.
        Action::SetTheme(crate::session::config::resolve_theme_name())
    }

    pub fn handle_dialog_click(&mut self, col: u16, row: u16) -> bool {
        if let Some(dialog) = &mut self.intro_dialog {
            let click = dialog.handle_click(col, row);
            let preview = dialog.take_pending_preview();
            if let Some(result) = click {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.intro_dialog = None;
                    }
                    DialogResult::Submit(outcome) => {
                        self.intro_dialog = None;
                        apply_intro_outcome(&outcome);
                        // No pending_intro_theme: the live preview
                        // already applied the chosen theme to
                        // `app.theme`; re-applying would force a
                        // terminal.clear() (the close-flash). Same
                        // rationale as the keyboard Submit branch.
                    }
                }
                if let Some(name) = preview {
                    self.pending_intro_theme = Some(name);
                }
                return true;
            }
            if let Some(name) = preview {
                self.pending_intro_theme = Some(name);
            }
            return true;
        }
        if let Some(dialog) = &mut self.unified_delete_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.unified_delete_dialog = None;
                    }
                    DialogResult::Submit(options) => {
                        self.unified_delete_dialog = None;
                        if let Err(e) = self.delete_selected(&options) {
                            tracing::error!(target: "tui.input", "Failed to delete session: {}", e);
                        }
                    }
                }
                return true;
            }
            // Click landed inside the dialog area but missed every
            // hit rect (e.g. on the title or border): swallow it so
            // the underlying list doesn't shift selection out from
            // under the modal.
            return true;
        }
        if let Some(dialog) = &mut self.new_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.new_dialog = None;
                    }
                    DialogResult::Submit(_data) => {
                        // The new-session dialog's submit is fired only
                        // by the Enter key path; clicks set focus or
                        // toggle the focused row and return Continue,
                        // never Submit. This arm is defensive.
                    }
                }
            }
            // Always swallow clicks while the new-session dialog is
            // open so the underlying list / preview don't react.
            return true;
        }
        // Confirm dialog floats over settings (e.g., the unsaved-changes
        // discard prompt), so it has to win over the settings-view
        // takeover for click routing the same way the keyboard path
        // checks `settings_close_confirm` ahead of `settings_view`.
        // Otherwise a click on Yes / No goes into settings and never
        // reaches the modal.
        if let Some(dialog) = &self.confirm_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                let action = dialog.action().to_string();
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.confirm_dialog = None;
                        self.pending_stop_session = None;
                        self.pending_force_remove_session = None;
                        // The settings close path mirrors the keyboard
                        // route: Cancel here means "don't discard," so
                        // settings stays open and `settings_close_confirm`
                        // resets to idle.
                        self.settings_close_confirm = false;
                    }
                    DialogResult::Submit(()) => {
                        let dont_ask_again = dialog.dont_ask_again();
                        self.confirm_dialog = None;
                        if self.settings_close_confirm {
                            // Discard branch: run the exact same sequence as the
                            // keyboard path, including the theme revert. Omitting
                            // it here stranded a live theme preview until the next
                            // restart when the user discarded via a mouse click.
                            self.pending_dialog_click_action =
                                Some(self.discard_settings_changes());
                        } else {
                            if action == "quit" && dont_ask_again {
                                self.disable_confirm_before_quit();
                            }
                            self.pending_dialog_click_action =
                                self.dispatch_confirm_submit(&action);
                        }
                    }
                }
            }
            // Always swallow clicks while the confirm dialog is open.
            return true;
        }
        if let Some(view) = &mut self.settings_view {
            // Settings is a full-screen takeover: every click inside
            // the area is for it, even when the click landed on the
            // background between widgets. `handle_click` mutates
            // focus / scope / selection on hits and returns None on
            // misses, but we still swallow the click either way.
            let _ = view.handle_click(col, row);
            return true;
        }
        if let Some(dialog) = &self.info_dialog {
            if let Some(DialogResult::Cancel) = dialog.handle_click(col, row) {
                self.info_dialog = None;
            }
            return true;
        }
        if let Some(dialog) = &self.update_confirm_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.update_confirm_dialog = None;
                    }
                    DialogResult::Submit(()) => {
                        let method = dialog.method.clone();
                        let version = dialog.latest_version.clone();
                        self.update_confirm_dialog = None;
                        self.pending_dialog_click_action =
                            Some(Action::SpawnUpdate(method, version));
                    }
                }
            }
            return true;
        }
        if let Some(dialog) = &self.changelog_dialog {
            if let Some(DialogResult::Submit(())) = dialog.handle_click(col, row) {
                self.changelog_dialog = None;
            }
            return true;
        }
        if let Some(dialog) = &self.telemetry_consent_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                let opt_in = match result {
                    DialogResult::Submit(opt_in) => Some(opt_in),
                    DialogResult::Cancel => Some(false),
                    DialogResult::Continue => None,
                };
                if let Some(opt_in) = opt_in {
                    self.telemetry_consent_dialog = None;
                    persist_telemetry_consent(opt_in);
                }
            }
            return true;
        }
        if let Some(dialog) = &self.snooze_duration_dialog {
            if let Some(DialogResult::Submit(minutes)) = dialog.handle_click(col, row) {
                self.snooze_duration_dialog = None;
                let sid = self.pending_snooze_session.take();
                if let Some(id) = sid {
                    if let Err(e) = self.snooze_session_for(&id, minutes) {
                        tracing::error!("snooze_session_for failed: {}", e);
                    }
                }
            }
            return true;
        }
        if let Some(dialog) = &self.no_agents_dialog {
            if let Some(DialogResult::Submit(action)) = dialog.handle_click(col, row) {
                match action {
                    NoAgentsAction::Recheck => {
                        let tools = crate::tmux::AvailableTools::detect();
                        if tools.any_available() {
                            self.set_available_tools(tools);
                            self.no_agents_dialog = None;
                        }
                    }
                    NoAgentsAction::Quit => {
                        self.no_agents_dialog = None;
                        self.pending_dialog_click_action = Some(Action::Quit);
                    }
                }
            }
            return true;
        }
        if let Some(picker) = &mut self.tool_picker_dialog {
            match picker.handle_click(col, row) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.tool_picker_dialog = None;
                }
                DialogResult::Submit(tool_name) => {
                    self.tool_picker_dialog = None;
                    self.view_mode = ViewMode::Tool(tool_name);
                    self.preview_scroll_offset = 0;
                    self.tool_preview_cache = super::PreviewCache::default();
                }
            }
            return true;
        }
        if let Some(dialog) = &mut self.group_delete_options_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.group_delete_options_dialog = None;
                    }
                    DialogResult::Submit(options) => {
                        self.group_delete_options_dialog = None;
                        if options.delete_sessions {
                            if let Err(e) = self.delete_group_with_sessions(&options) {
                                tracing::error!(target: "tui.input", "Failed to delete group with sessions: {}", e);
                            }
                        } else if let Err(e) = self.delete_selected_group() {
                            tracing::error!(target: "tui.input", "Failed to delete group: {}", e);
                        }
                    }
                }
            }
            return true;
        }
        if let Some(dialog) = &mut self.rename_dialog {
            // The rename dialog click handler only ever returns Continue;
            // submitting requires the user to press Enter on a valid input.
            // Always swallow the click so the underlying list doesn't react.
            let _ = dialog.handle_click(col, row);
            return true;
        }
        if self.worktree_name_dialog.is_some() {
            // Keyboard-driven dialog; swallow clicks so the list underneath
            // doesn't react while it's open.
            return true;
        }
        if let Some(dialog) = &mut self.restart_dialog {
            let _ = dialog.handle_click(col, row);
            return true;
        }
        if let Some(dialog) = &mut self.sort_picker_dialog {
            match dialog.handle_click(col, row) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.sort_picker_dialog = None;
                }
                DialogResult::Submit(order) => {
                    self.sort_picker_dialog = None;
                    if order != self.sort_order {
                        self.apply_sort_order(order);
                    }
                }
            }
            return true;
        }
        if let Some(dialog) = &mut self.group_picker_dialog {
            match dialog.handle_click(col, row) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.group_picker_dialog = None;
                }
                DialogResult::Submit(mode) => {
                    self.group_picker_dialog = None;
                    if mode != self.group_by {
                        self.apply_group_by(mode);
                    }
                }
            }
            return true;
        }
        if let Some(dialog) = &mut self.project_session_picker_dialog {
            match dialog.handle_click(col, row) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.project_session_picker_dialog = None;
                }
                DialogResult::Submit(path) => {
                    self.project_session_picker_dialog = None;
                    self.open_new_session_dialog();
                    if let Some(d) = &mut self.new_dialog {
                        d.set_path(path);
                    }
                }
            }
            return true;
        }
        if let Some(palette) = &mut self.command_palette {
            match palette.handle_click(col, row) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.command_palette = None;
                }
                DialogResult::Submit(action) => {
                    self.command_palette = None;
                    // No `update_info` here (the mouse handler doesn't
                    // thread it through); palette commands that rely on
                    // it (Update prompts) only do so from the keyboard
                    // path. The fallback is harmless.
                    self.pending_dialog_click_action = self.dispatch_palette_action(action, None);
                }
            }
            return true;
        }
        if let Some(dialog) = &self.hooks_install_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.hooks_install_dialog = None;
                        self.pending_hooks_install_data = None;
                    }
                    DialogResult::Submit(_) => {
                        self.hooks_install_dialog = None;
                        if let Ok(mut config) =
                            crate::session::config::load_config().map(|c| c.unwrap_or_default())
                        {
                            config.app_state.has_acknowledged_agent_hooks = true;
                            if let Err(e) = crate::session::config::save_config(&config) {
                                tracing::warn!(target: "tui.input", "Failed to save config: {e}");
                            }
                        }
                        if let Some(data) = self.pending_hooks_install_data.take() {
                            self.pending_dialog_click_action =
                                self.maybe_confirm_volume_ignores_globs(data);
                        }
                    }
                }
            }
            return true;
        }
        if let Some(dialog) = &self.volume_ignores_glob_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                let dont_ask_again = dialog.dont_ask_again();
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.volume_ignores_glob_dialog = None;
                        self.pending_volume_ignores_glob_data = None;
                    }
                    DialogResult::Submit(_) => {
                        self.volume_ignores_glob_dialog = None;
                        if dont_ask_again {
                            self.persist_volume_ignores_globs_ack();
                        }
                        if let Some(data) = self.pending_volume_ignores_glob_data.take() {
                            self.pending_dialog_click_action = self.continue_session_creation(data);
                        }
                    }
                }
            }
            return true;
        }
        if let Some(dialog) = &self.repo_trust_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.repo_trust_dialog = None;
                        self.pending_repo_trust_data = None;
                    }
                    DialogResult::Submit(action) => {
                        self.repo_trust_dialog = None;
                        if let Some(data) = self.pending_repo_trust_data.take() {
                            let emit = match action {
                                RepoTrustAction::Trust {
                                    hooks_hash,
                                    mcp_hash,
                                    project_path,
                                    hooks,
                                } => {
                                    // If persisting trust fails, abort creation:
                                    // launching anyway leaves a split state where
                                    // hooks are treated as approved but project MCP
                                    // stays gated off (it is read back from the
                                    // unwritten hashes).
                                    if let Err(e) = repo_config::trust_repo(
                                        std::path::Path::new(&project_path),
                                        hooks_hash.as_deref(),
                                        mcp_hash.as_deref(),
                                    ) {
                                        tracing::error!(target: "tui.input", "Failed to persist repo trust; aborting session creation: {}", e);
                                        None
                                    } else {
                                        self.create_session_with_hooks(data, hooks)
                                    }
                                }
                                RepoTrustAction::Skip { hooks } => {
                                    self.create_session_with_hooks(data, hooks)
                                }
                            };
                            self.pending_dialog_click_action = emit;
                        }
                    }
                }
            }
            return true;
        }
        // Other dialogs also need to swallow clicks while open. The
        // existing `has_dialog()` gates inside list / preview / divider
        // handlers already do this, so no extra work here.
        false
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        update_info: Option<&crate::update::UpdateInfo>,
    ) -> Option<Action> {
        // Any keystroke drops a finalized preview-pane selection. The
        // highlight pins to cell coords, so as soon as the user starts
        // doing anything else (navigating the list, opening a dialog,
        // typing through live-send, etc.) the cells underneath can
        // change and the highlight would point at unrelated content.
        // Doing the clear here covers both the live-send branch below
        // and the regular home-view path.
        self.clear_preview_selection();

        // Live-send capture normally wins over every other key handler:
        // the home view acts as a thin relay to the target pane, so
        // dialog hotkeys, search, and list navigation all suspend until
        // the user exits with Ctrl+q. That works when dialogs are only
        // reachable via keyboard (the hotkey itself gets swallowed by
        // live-send so the dialog never opens). Once a non-live-send
        // overlay HAS been opened; via the empty-sidebar click that
        // pops the new-session dialog, a right-click context menu, or
        // any future click-to-open path; its keys must go to the
        // overlay, not the underlying tmux pane. Otherwise the user
        // sees the dialog but Esc / Enter / typed characters silently
        // get routed to the session behind it.
        if self.live_send.is_some() && !self.has_non_live_send_overlay() {
            self.handle_live_send_key(key);
            return None;
        }

        // Handle unsaved changes confirmation for settings (shown over settings view)
        if self.settings_close_confirm {
            if let Some(dialog) = &mut self.confirm_dialog {
                match dialog.handle_key(key) {
                    DialogResult::Continue => return None,
                    DialogResult::Cancel => {
                        // User chose not to discard, go back to settings
                        self.confirm_dialog = None;
                        self.settings_close_confirm = false;
                        return None;
                    }
                    DialogResult::Submit(_) => {
                        // User chose to discard changes
                        return Some(self.discard_settings_changes());
                    }
                }
            }
        }

        // Handle settings view (full-screen takeover)
        if let Some(ref mut settings) = self.settings_view {
            match settings.handle_key(key) {
                SettingsAction::Continue => {
                    return None;
                }
                SettingsAction::Close => {
                    self.settings_view = None;
                    // Refresh config-dependent state in case settings changed
                    self.refresh_from_config();
                    // Reload the theme from the global config (theme is a global
                    // preference, not profile-merged) so the repaint matches boot.
                    return Some(Action::SetTheme(
                        crate::session::config::resolve_theme_name(),
                    ));
                }
                SettingsAction::UnsavedChangesWarning => {
                    // Show confirmation dialog
                    self.confirm_dialog = Some(ConfirmDialog::new(
                        "Unsaved Changes",
                        "You have unsaved changes. Discard them?",
                        "discard_settings",
                    ));
                    self.settings_close_confirm = true;
                    return None;
                }
                SettingsAction::PreviewTheme(name) => {
                    return Some(Action::SetTheme(name));
                }
            }
        }

        // Handle diff view (full-screen takeover)
        if let Some(ref mut diff_view) = self.diff_view {
            let action = diff_view.handle_key(key);
            if let Some((session_id, new_override)) = diff_view.take_pending_override() {
                if let Err(e) = self.apply_user_action(&session_id, |inst| {
                    inst.base_branch_override = new_override.clone();
                }) {
                    tracing::warn!(
                        target: "tui.home",
                        "Failed to persist base_branch_override: {}",
                        e
                    );
                }
            }
            match action {
                DiffAction::Continue => return None,
                DiffAction::Close => {
                    self.diff_view = None;
                    return None;
                }
                DiffAction::EditFile(path) => {
                    return Some(Action::EditFile(path));
                }
            }
        }

        // Handle serve view (full-screen takeover)
        #[cfg(feature = "serve")]
        if let Some(ref mut serve) = self.serve_view {
            match serve.handle_key(key) {
                ServeAction::Continue => return None,
                ServeAction::Close => {
                    self.serve_view = None;
                    return None;
                }
            }
        }

        // Right-click context menu. Routed before every other dialog so
        // keys go to the popup that the user just opened on top of the
        // sidebar. Submit dispatches through the shared helper so the
        // keyboard and the mouse-click path stay byte-for-byte aligned.
        if let Some(menu) = &mut self.context_menu {
            match menu.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.context_menu = None;
                }
                DialogResult::Submit(action) => {
                    self.context_menu = None;
                    self.dispatch_context_menu_action(action);
                }
            }
            return None;
        }

        // Handle no-agents dialog (highest priority, blocks all interaction)
        if let Some(dialog) = &mut self.no_agents_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel | DialogResult::Submit(NoAgentsAction::Quit) => {
                    return Some(Action::Quit);
                }
                DialogResult::Submit(NoAgentsAction::Recheck) => {
                    let tools = crate::tmux::AvailableTools::detect();
                    if tools.any_available() {
                        self.set_available_tools(tools);
                        self.no_agents_dialog = None;
                    }
                    // If still no agents, keep dialog open (user can try again)
                }
            }
            return None;
        }

        // Handle intro/changelog dialogs first (highest priority).
        // Intro live-previews themes as the cursor moves; drain any pending
        // preview the dialog queued and emit it as Action::SetTheme so the
        // root App switches themes without round-tripping through the
        // settings view.
        if let Some(dialog) = &mut self.intro_dialog {
            let result = dialog.handle_key(key);
            let preview = dialog.take_pending_preview();
            match result {
                DialogResult::Continue => {
                    if let Some(name) = preview {
                        return Some(Action::SetTheme(name));
                    }
                    return None;
                }
                DialogResult::Cancel => {
                    self.intro_dialog = None;
                    if let Some(name) = preview {
                        return Some(Action::SetTheme(name));
                    }
                    return None;
                }
                DialogResult::Submit(outcome) => {
                    self.intro_dialog = None;
                    apply_intro_outcome(&outcome);
                    // No SetTheme dispatch: the live preview already
                    // applied the chosen theme to `app.theme` while the
                    // user was on the picker page. Re-dispatching here
                    // would only re-trigger `set_theme → needs_redraw`,
                    // which forces a `terminal.clear()` on the next loop
                    // iteration — the close-flash the user sees.
                    return None;
                }
            }
        }

        if let Some(dialog) = &mut self.changelog_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel | DialogResult::Submit(_) => {
                    self.changelog_dialog = None;
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.telemetry_consent_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Submit(opt_in) => {
                    self.telemetry_consent_dialog = None;
                    persist_telemetry_consent(opt_in);
                }
                // Cancel can't be produced by this dialog (Esc maps to a
                // decline), but treat it as a decline for completeness.
                DialogResult::Cancel => {
                    self.telemetry_consent_dialog = None;
                    persist_telemetry_consent(false);
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.info_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel | DialogResult::Submit(_) => {
                    self.info_dialog = None;
                    if let Some(session_id) = self.pending_attach_after_warning.take() {
                        return Some(Action::AttachSession(session_id));
                    }
                }
            }
            return None;
        }

        // Command palette captures input ahead of the help overlay so its own
        // Esc/Enter/text keys reach it without going through the action match.
        if let Some(palette) = &mut self.command_palette {
            match palette.handle_key(key) {
                DialogResult::Continue => return None,
                DialogResult::Cancel => {
                    self.command_palette = None;
                    return None;
                }
                DialogResult::Submit(action) => {
                    self.command_palette = None;
                    return self.dispatch_palette_action(action, update_info);
                }
            }
        }

        // Handle tool picker dialog
        if let Some(picker) = &mut self.tool_picker_dialog {
            match picker.handle_key(key) {
                DialogResult::Continue => return None,
                DialogResult::Cancel => {
                    self.tool_picker_dialog = None;
                    return None;
                }
                DialogResult::Submit(tool_name) => {
                    self.tool_picker_dialog = None;
                    self.view_mode = ViewMode::Tool(tool_name);
                    self.preview_scroll_offset = 0;
                    self.tool_preview_cache = super::PreviewCache::default();
                    return None;
                }
            }
        }

        if let Some(dialog) = &mut self.snooze_duration_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.snooze_duration_dialog = None;
                    self.pending_snooze_session = None;
                }
                DialogResult::Submit(minutes) => {
                    self.snooze_duration_dialog = None;
                    let sid = self.pending_snooze_session.take();
                    if let Some(id) = sid {
                        if let Err(e) = self.snooze_session_for(&id, minutes) {
                            tracing::error!("snooze_session_for failed: {}", e);
                        }
                    }
                }
            }
            return None;
        }

        // Handle other dialog input
        if self.show_help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.show_help = false;
                    self.help_scroll = 0;
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    self.help_scroll = self.help_scroll.saturating_add(10);
                }
                KeyCode::PageUp => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.help_scroll = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    // u16::MAX overshoots intentionally; HelpOverlay::render
                    // clamps to the actual max scroll for the current layout.
                    self.help_scroll = u16::MAX;
                }
                _ => {}
            }
            return None;
        }

        if let Some(dialog) = &mut self.hooks_install_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.hooks_install_dialog = None;
                    self.pending_hooks_install_data = None;
                }
                DialogResult::Submit(_) => {
                    self.hooks_install_dialog = None;
                    // Persist the acknowledgment
                    if let Ok(mut config) =
                        crate::session::config::load_config().map(|c| c.unwrap_or_default())
                    {
                        config.app_state.has_acknowledged_agent_hooks = true;
                        if let Err(e) = crate::session::config::save_config(&config) {
                            tracing::warn!(target: "tui.input", "Failed to save config: {e}");
                        }
                    }
                    // Resume session creation
                    if let Some(data) = self.pending_hooks_install_data.take() {
                        return self.maybe_confirm_volume_ignores_globs(data);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.volume_ignores_glob_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.volume_ignores_glob_dialog = None;
                    self.pending_volume_ignores_glob_data = None;
                }
                DialogResult::Submit(_) => {
                    let dont_ask_again = dialog.dont_ask_again();
                    self.volume_ignores_glob_dialog = None;
                    if dont_ask_again {
                        self.persist_volume_ignores_globs_ack();
                    }
                    if let Some(data) = self.pending_volume_ignores_glob_data.take() {
                        return self.continue_session_creation(data);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.repo_trust_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.repo_trust_dialog = None;
                    self.pending_repo_trust_data = None;
                }
                DialogResult::Submit(action) => {
                    self.repo_trust_dialog = None;
                    if let Some(data) = self.pending_repo_trust_data.take() {
                        match action {
                            RepoTrustAction::Trust {
                                hooks_hash,
                                mcp_hash,
                                project_path,
                                hooks,
                            } => {
                                // Abort creation if trust cannot be persisted, to
                                // avoid a split state (hooks approved but project
                                // MCP gated off the unwritten hashes).
                                if let Err(e) = repo_config::trust_repo(
                                    std::path::Path::new(&project_path),
                                    hooks_hash.as_deref(),
                                    mcp_hash.as_deref(),
                                ) {
                                    tracing::error!(target: "tui.input", "Failed to persist repo trust; aborting session creation: {}", e);
                                    return None;
                                }
                                return self.create_session_with_hooks(data, hooks);
                            }
                            RepoTrustAction::Skip { hooks } => {
                                return self.create_session_with_hooks(data, hooks);
                            }
                        }
                    }
                }
            }
            return None;
        }

        let dialog_result = self
            .new_dialog
            .as_mut()
            .map(|dialog| dialog.handle_key(key));

        if let Some(result) = dialog_result {
            match result {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    // If creation is pending, mark it as cancelled
                    if self.is_creation_pending() {
                        self.cancel_creation();
                    } else {
                        self.new_dialog = None;
                    }
                }
                DialogResult::Submit(data) => {
                    // Check if the tool uses hooks and user hasn't acknowledged yet
                    let tool_name = if data.tool.is_empty() {
                        "claude".to_string()
                    } else {
                        data.tool.clone()
                    };

                    let resolved_config = resolve_config_or_warn(&data.profile);
                    if let Some(hook_agent) =
                        resolve_hook_install_agent(&tool_name, &resolved_config.session)
                    {
                        let config = crate::session::config::load_config().ok().flatten();
                        let hooks_enabled = resolved_config.session.agent_status_hooks;
                        let acknowledged = config
                            .as_ref()
                            .map(|c| c.app_state.has_acknowledged_agent_hooks)
                            .unwrap_or(false);

                        if hooks_enabled && !acknowledged {
                            self.hooks_install_dialog = Some(HooksInstallDialog::new_for_profile(
                                hook_agent.name,
                                Some(&data.profile),
                            ));
                            self.pending_hooks_install_data = Some(data);
                            return None;
                        }
                    }

                    return self.maybe_confirm_volume_ignores_globs(data);
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.confirm_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.confirm_dialog = None;
                    self.pending_stop_session = None;
                    self.pending_force_remove_session = None;
                }
                DialogResult::Submit(_) => {
                    let action = dialog.action().to_string();
                    let dont_ask_again = dialog.dont_ask_again();
                    self.confirm_dialog = None;
                    if action == "quit" && dont_ask_again {
                        self.disable_confirm_before_quit();
                    }
                    if let Some(emit) = self.dispatch_confirm_submit(&action) {
                        return Some(emit);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.unified_delete_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.unified_delete_dialog = None;
                }
                DialogResult::Submit(options) => {
                    self.unified_delete_dialog = None;
                    if let Err(e) = self.delete_selected(&options) {
                        tracing::error!(target: "tui.input", "Failed to delete session: {}", e);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.group_delete_options_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.group_delete_options_dialog = None;
                }
                DialogResult::Submit(options) => {
                    self.group_delete_options_dialog = None;
                    if options.delete_sessions {
                        if let Err(e) = self.delete_group_with_sessions(&options) {
                            tracing::error!(target: "tui.input", "Failed to delete group with sessions: {}", e);
                        }
                    } else if let Err(e) = self.delete_selected_group() {
                        tracing::error!(target: "tui.input", "Failed to delete group: {}", e);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.rename_dialog {
            let mode = dialog.mode();
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.rename_dialog = None;
                    self.group_rename_context = None;
                }
                DialogResult::Submit(data) => {
                    self.rename_dialog = None;
                    match mode {
                        RenameMode::Session => {
                            if let Err(e) = self.rename_selected(
                                &data.title,
                                data.group.as_deref(),
                                data.profile.as_deref(),
                            ) {
                                tracing::error!(target: "tui.input", "Failed to rename session: {}", e);
                            }
                        }
                        RenameMode::Group => {
                            if let Err(e) = self.rename_selected_group(
                                data.group.as_deref(),
                                data.profile.as_deref(),
                            ) {
                                tracing::error!(target: "tui.input", "Failed to rename group: {}", e);
                            }
                        }
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.worktree_name_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.worktree_name_dialog = None;
                }
                DialogResult::Submit(data) => {
                    self.worktree_name_dialog = None;
                    if let Err(e) =
                        self.set_worktree_name_for_selected(&data.name, data.rename_branch)
                    {
                        self.info_dialog = Some(InfoDialog::new(
                            "Edit Workdir Name Failed",
                            &format!("Could not edit the workdir name: {e}"),
                        ));
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.restart_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.restart_dialog = None;
                }
                DialogResult::Submit(data) => {
                    self.restart_dialog = None;
                    let profile = data.profile.as_deref();
                    let tool = data.tool.as_deref();
                    let extra_args = data.extra_args.as_deref();
                    let command_override = data.command_override.as_deref();
                    if let Err(e) =
                        self.restart_selected_session(profile, tool, extra_args, command_override)
                    {
                        // Surface the restart error to the user via the
                        // InfoDialog rather than only the debug log; the
                        // user explicitly initiated this action and needs
                        // to know it failed.
                        tracing::warn!("restart_selected_session failed: {}", e);
                        self.info_dialog = Some(InfoDialog::new(
                            "Restart Failed",
                            &format!("Could not restart session: {e}"),
                        ));
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.projects_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel | DialogResult::Submit(()) => {
                    self.projects_dialog = None;
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.group_picker_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.group_picker_dialog = None;
                }
                DialogResult::Submit(mode) => {
                    self.group_picker_dialog = None;
                    if mode != self.group_by {
                        self.apply_group_by(mode);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.project_session_picker_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.project_session_picker_dialog = None;
                }
                DialogResult::Submit(path) => {
                    self.project_session_picker_dialog = None;
                    self.open_new_session_dialog();
                    if let Some(d) = &mut self.new_dialog {
                        d.set_path(path);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.sort_picker_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.sort_picker_dialog = None;
                }
                DialogResult::Submit(order) => {
                    self.sort_picker_dialog = None;
                    if order != self.sort_order {
                        self.apply_sort_order(order);
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.profile_picker_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.profile_picker_dialog = None;
                }
                DialogResult::Submit(action) => match action {
                    ProfilePickerAction::Switch(name) => {
                        self.profile_picker_dialog = None;
                        // The synthetic "all" entry (only present in filtered mode)
                        // switches back to all-profiles mode
                        let profile = if self.active_profile.is_some() && name == "all" {
                            None
                        } else {
                            Some(name)
                        };
                        if let Err(e) = self.switch_profile(profile) {
                            tracing::error!(target: "tui.input", "Failed to switch profile: {}", e);
                        }
                    }
                    ProfilePickerAction::Created(name) => {
                        self.profile_picker_dialog = None;
                        match crate::session::create_profile(&name) {
                            Ok(()) => {
                                if let Err(e) = self.switch_profile(Some(name)) {
                                    tracing::error!(target: "tui.input", "Failed to switch to new profile: {}", e);
                                }
                            }
                            Err(e) => {
                                self.info_dialog = Some(InfoDialog::new(
                                    "Error",
                                    &format!("Failed to create profile: {}", e),
                                ));
                            }
                        }
                    }
                    ProfilePickerAction::Deleted(name) => {
                        match crate::session::delete_profile(&name) {
                            Ok(()) => {
                                if let Some(entry) = self.disk_watch_handles.remove(&name) {
                                    super::drop_disk_watch_entry(entry);
                                }
                                self.show_profile_picker();
                            }
                            Err(e) => {
                                self.profile_picker_dialog = None;
                                self.info_dialog = Some(InfoDialog::new(
                                    "Error",
                                    &format!("Failed to delete profile: {}", e),
                                ));
                            }
                        }
                    }
                },
            }
            return None;
        }

        // Send message dialog
        if let Some(dialog) = &mut self.send_message_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.send_message_dialog = None;
                    self.pending_send_session = None;
                    self.pending_send_target = live_send::LiveSendTarget::Agent;
                }
                DialogResult::Submit(message) => {
                    self.send_message_dialog = None;
                    if let Some(session_id) = self.pending_send_session.take() {
                        // Defer the actual work to execute_action so the app
                        // loop can render a status indicator first. The send
                        // path may need to start a Docker container or wait
                        // for an agent splash to settle (up to several seconds
                        // total); doing it inline here would freeze the TUI
                        // with no feedback.
                        return Some(Action::SendMessage(session_id, message));
                    }
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.update_confirm_dialog {
            use crate::tui::dialogs::DialogResult;
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.update_confirm_dialog = None;
                }
                DialogResult::Submit(()) => {
                    let method = dialog.method.clone();
                    let version = dialog.latest_version.clone();
                    self.update_confirm_dialog = None;
                    return Some(Action::SpawnUpdate(method, version));
                }
            }
            return None;
        }

        // Search mode. Intentionally takes priority over the Ctrl+K palette
        // binding below: while the search input is focused, every key (including
        // Ctrl+K) feeds the search box. Users can press Esc to exit search and
        // then open the palette. Don't move this block past the Ctrl+K check
        // unless you want palette activation to clobber search input.
        if self.search_active {
            match key.code {
                KeyCode::Esc => {
                    self.search_active = false;
                    self.search_query = Input::default();
                    self.search_matches.clear();
                    self.search_match_index = 0;
                }
                KeyCode::Enter => {
                    self.search_active = false;
                    self.search_query = Input::default();
                    self.search_matches.clear();
                    self.search_match_index = 0;
                }
                _ => {
                    self.search_query
                        .handle_event(&crossterm::event::Event::Key(key));
                    self.update_search();
                }
            }
            return None;
        }

        // Ctrl+K opens the command palette regardless of strict-hotkey mode.
        // Activated here (before strict normalization) so the binding stays
        // discoverable on every keymap.
        if matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.open_command_palette();
            return None;
        }

        // Strict-mode relocation is handled inside dispatch_action_key via the
        // shared bindings registry (resolve picks the right chord per mode and
        // a typing-guard catches unbound bare lowercase), so no key rewriting
        // happens here.
        self.dispatch_action_key(key, update_info)
    }

    /// Run the main action dispatch on a key.
    ///
    /// Extracted from `handle_key` so the command palette can route through the
    /// same code path. Relocatable action keys are resolved through the shared
    /// [`bindings`](super::bindings) registry, so the dispatcher, palette, and
    /// help overlay can't drift on which chord (or mode) an action binds to.
    /// Pure navigation/structural keys, which never relocate between modes,
    /// stay as explicit arms below and are tried after the registry. The
    /// strict-mode typing-guard is the final fallback.
    fn dispatch_action_key(
        &mut self,
        key: KeyEvent,
        update_info: Option<&crate::update::UpdateInfo>,
    ) -> Option<Action> {
        // Dynamic tool session hotkeys (checked before everything else).
        if let Some(tool_name) = self.match_tool_hotkey(&key) {
            if matches!(&self.view_mode, ViewMode::Tool(current) if current == &tool_name) {
                self.view_mode = ViewMode::Structured;
            } else {
                self.view_mode = ViewMode::Tool(tool_name);
                self.preview_scroll_offset = 0;
                self.tool_preview_cache = super::PreviewCache::default();
            }
            return None;
        }

        // Context-dependent Esc handling (not a relocatable action).
        match key.code {
            KeyCode::Esc if !self.search_matches.is_empty() => {
                self.search_matches.clear();
                self.search_match_index = 0;
                self.search_query = Input::default();
                return None;
            }
            KeyCode::Esc if matches!(self.view_mode, ViewMode::Tool(_)) => {
                self.view_mode = ViewMode::Structured;
                return None;
            }
            _ => {}
        }

        // Registry-driven action keys.
        let ctx = bindings::Ctx {
            view_mode: self.view_mode.clone(),
            sort_order: self.sort_order,
            has_search: !self.search_matches.is_empty(),
            project_group_selected: self.project_group_at_cursor().is_some(),
        };
        if let Some(id) = bindings::resolve(&key, self.strict_hotkeys, &ctx) {
            return self.run_action(id, update_info);
        }

        // Navigation / structural keys: identical in both modes, never relocate.
        match key.code {
            KeyCode::Up if key.modifiers.contains(KeyModifiers::SHIFT) => self.move_cursor(-10),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::SHIFT) => self.move_cursor(10),
            KeyCode::Char('{') => self.move_cursor(-10),
            KeyCode::Char('}') => self.move_cursor(10),
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-10),
            KeyCode::PageDown => self.move_cursor(10),
            KeyCode::Home => {
                self.cursor = 0;
                self.mouse_pos = None;
                self.update_selected();
            }
            KeyCode::End | KeyCode::Char('G') if !self.flat_items.is_empty() => {
                self.cursor = self.flat_items.len() - 1;
                self.mouse_pos = None;
                self.update_selected();
            }
            KeyCode::Char('<') => self.shrink_list(),
            KeyCode::Char('>') => self.grow_list(),
            KeyCode::Enter => {
                if self.selected_session.is_some() {
                    return self.activate_selected_session();
                } else if let Some(Item::Group { path, .. }) = self.flat_items.get(self.cursor) {
                    let path = path.clone();
                    self.toggle_group_collapsed(&path);
                }
            }
            // Tab is the activation key's complement: it does whichever of
            // (live-send, tmux attach) `Enter` doesn't. Only fires when
            // `live_send` is None (the live-send capture short-circuits above).
            KeyCode::Tab => {
                let swap_to_attach = self
                    .selected_session
                    .as_deref()
                    .map(|id| {
                        matches!(
                            self.default_attach_mode(id),
                            Some(crate::session::NewSessionAttachMode::LiveSend)
                        )
                    })
                    .unwrap_or(false);
                if swap_to_attach {
                    if let Some(action) = self.tab_attach_action() {
                        return Some(action);
                    }
                } else if let Some(action) = self.start_live_send() {
                    return Some(action);
                }
            }
            KeyCode::Left | KeyCode::Char('h') => {
                if let Some(Item::Group {
                    path, collapsed, ..
                }) = self.flat_items.get(self.cursor)
                {
                    if !collapsed {
                        let path = path.clone();
                        self.toggle_group_collapsed(&path);
                    }
                }
            }
            KeyCode::Right | KeyCode::Char('l') => {
                if let Some(Item::Group {
                    path, collapsed, ..
                }) = self.flat_items.get(self.cursor)
                {
                    if *collapsed {
                        let path = path.clone();
                        self.toggle_group_collapsed(&path);
                    }
                }
            }
            // Strict-mode typing guard: any bare lowercase letter not bound to
            // an action or navigation key opens the compose dialog pre-filled
            // with that character (the no-destructive-lowercase contract).
            KeyCode::Char(c)
                if self.strict_hotkeys
                    && key.modifiers == KeyModifiers::NONE
                    && c.is_ascii_lowercase() =>
            {
                self.capture_letter_to_compose(c);
            }
            _ => {}
        }
        None
    }

    /// Execute a resolved [`ActionId`]. The single home for each action's
    /// behavior: the keyboard dispatcher and the command palette both route
    /// here, so they can't diverge on what an action does.
    fn run_action(
        &mut self,
        id: ActionId,
        update_info: Option<&crate::update::UpdateInfo>,
    ) -> Option<Action> {
        match id {
            ActionId::Quit => return Some(Action::Quit),
            ActionId::Help => {
                self.show_help = true;
                self.help_scroll = 0;
            }
            ActionId::ToolPicker => {
                if matches!(self.view_mode, ViewMode::Tool(_)) {
                    self.view_mode = ViewMode::Structured;
                } else if !self.tool_configs.is_empty() {
                    self.open_tool_picker();
                }
            }
            ActionId::SearchStart => {
                self.search_active = true;
                self.search_query = Input::default();
            }
            ActionId::SearchNext => {
                if self.search_matches.is_empty() {
                    return None;
                }
                self.search_match_index = (self.search_match_index + 1) % self.search_matches.len();
                self.cursor = self.search_matches[self.search_match_index];
                self.update_selected();
            }
            ActionId::SearchPrev => {
                if self.search_matches.is_empty() {
                    return None;
                }
                self.search_match_index = if self.search_match_index == 0 {
                    self.search_matches.len() - 1
                } else {
                    self.search_match_index - 1
                };
                self.cursor = self.search_matches[self.search_match_index];
                self.update_selected();
            }
            ActionId::NewSession => self.open_new_session_dialog(),
            ActionId::NewFromSelection => self.open_new_from_selection(),
            ActionId::NewFromProject => self.open_project_session_picker(),
            ActionId::AttachTerminal => return self.attach_terminal_for_selected(),
            ActionId::ToggleView => {
                self.view_mode = match self.view_mode {
                    ViewMode::Structured => ViewMode::Terminal,
                    ViewMode::Terminal | ViewMode::Tool(_) => ViewMode::Structured,
                };
            }
            ActionId::SendMessage => self.open_send_message_dialog(),
            ActionId::Stop => self.stop_selected(),
            ActionId::Delete => self.open_delete_for_selected(),
            ActionId::Rename => self.open_rename_for_selected(),
            ActionId::SetWorktreeName => self.open_worktree_name_for_selected(),
            ActionId::Diff => self.open_diff_for_selected(),
            ActionId::Serve => self.open_serve(),
            ActionId::Settings => self.open_settings(),
            ActionId::Profiles => self.show_profile_picker(),
            ActionId::Projects => {
                let profile = self.config_profile();
                self.projects_dialog = Some(ProjectsDialog::new(&profile));
            }
            ActionId::Restart => self.open_restart_dialog(),
            ActionId::Update => return self.run_update(update_info),
            ActionId::ToggleArchive => {
                if self.selected_group.is_some() {
                    self.prompt_archive_selected_group();
                } else if let Err(e) = self.toggle_archive_at_cursor() {
                    tracing::error!("toggle_archive_at_cursor failed: {}", e);
                }
            }
            ActionId::ToggleFavorite => {
                if let Err(e) = self.toggle_favorite_at_cursor() {
                    tracing::error!("toggle_favorite_at_cursor failed: {}", e);
                }
            }
            ActionId::ToggleSnooze => {
                if let Err(e) = self.toggle_snooze_at_cursor() {
                    tracing::error!("toggle_snooze_at_cursor failed: {}", e);
                }
            }
            ActionId::ToggleContainer => self.toggle_container_for_selected(),
            ActionId::TogglePreviewInfo => self.toggle_preview_info(),
            ActionId::SortPicker => self.show_sort_picker(),
            ActionId::GroupBy => self.show_group_picker(),
            ActionId::ToggleProjectPin => self.toggle_project_pin_at_cursor(),
            ActionId::NextWaiting => self.jump_to_next_waiting(),
        }
        None
    }

    fn open_new_from_selection(&mut self) {
        if self.creating_stub_id.is_some() {
            self.info_dialog = Some(InfoDialog::new(
                "Please Wait",
                "A session is already being created. Wait for it to finish or press Ctrl+C to cancel.",
            ));
            return;
        }
        // Same gate as `open_new_session_dialog`: with no agent available the
        // dialog has nothing to create, so point the user at setup instead of
        // opening an unusable form. Keeps `'N'` and the group menu's New
        // Session in step with `'n'` and the empty-sidebar menu.
        if !self.available_tools.any_available() {
            self.show_no_agents();
            return;
        }
        let prefill_path = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .map(|inst| inst.repo_path().to_string())
            .or_else(|| {
                // No session selected (the project/group right-click menu, or
                // `'N'` on a group header): borrow a member's repo path so the
                // new session lands in the same project, matching the web
                // sidebar's per-project "+".
                self.selected_group
                    .as_ref()
                    .and_then(|g| self.group_repo_path(g))
            });
        let prefill_group = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .and_then(|inst| {
                if inst.group_path.is_empty() {
                    None
                } else {
                    Some(inst.group_path.clone())
                }
            })
            .or_else(|| self.selected_group.clone());

        if prefill_path.is_some() || prefill_group.is_some() {
            let existing_groups: Vec<String> =
                self.all_groups().iter().map(|g| g.path.clone()).collect();
            let current_profile = self
                .profile_for_cursor(self.cursor)
                .unwrap_or_else(|| self.config_profile());
            let profiles = list_profiles().unwrap_or_else(|_| vec![current_profile.clone()]);
            let mut dialog = NewSessionDialog::new(
                self.available_tools.clone(),
                existing_groups,
                &current_profile,
                profiles,
            );
            let has_prefilled_path = prefill_path.is_some();
            if let Some(path) = prefill_path {
                dialog.set_path(path);
            }
            if let Some(group) = prefill_group {
                dialog.set_group(group);
            }
            // Skip to the title whenever the path is genuinely prefilled,
            // whether inherited from a session or borrowed from a project/group
            // member, so the user lands directly on naming. Only an empty group
            // (no member to borrow a path from) leaves focus on the default cwd
            // so the user can confirm it.
            if has_prefilled_path {
                dialog.focus_title();
            }
            self.new_dialog = Some(dialog);
        }
    }

    /// Pick a representative repo path for a selected group so "New Session"
    /// from a project/group can prefill the working directory. In project mode
    /// the group label is a derived basename, so match members by
    /// `project_group_name`; in manual mode match by the stored `group_path`,
    /// including nested subgroups. Returns `None` for an empty group (no member
    /// to borrow a path from), leaving the dialog on the default cwd.
    pub(super) fn group_repo_path(&self, group_path: &str) -> Option<String> {
        self.instances
            .iter()
            .find(|inst| match self.group_by {
                GroupByMode::Project => super::project_group_name(inst) == group_path,
                _ => {
                    inst.group_path == group_path
                        || inst.group_path.starts_with(&format!("{group_path}/"))
                }
            })
            .map(|inst| inst.repo_path().to_string())
            .or_else(|| {
                // An empty pinned project has no member sessions to borrow a
                // path from; fall back to the registered project's path so
                // "New Session" can still launch under it (the point of pinning
                // an empty project).
                if self.group_by == GroupByMode::Project {
                    self.registered_projects
                        .iter()
                        .find(|p| crate::session::projects::repo_label(&p.path) == group_path)
                        .map(|p| p.path.clone())
                } else {
                    None
                }
            })
    }

    fn attach_terminal_for_selected(&mut self) -> Option<Action> {
        // Quick-attach to paired terminal from any view.
        if let Some(id) = &self.selected_session {
            if let Some(inst) = self.get_instance(id) {
                if matches!(inst.status, Status::Deleting | Status::Creating) {
                    return None;
                }
            }
            let terminal_mode = if let Some(inst) = self.get_instance(id) {
                if inst.is_sandboxed() {
                    self.get_terminal_mode(id)
                } else {
                    TerminalMode::Host
                }
            } else {
                TerminalMode::Host
            };
            return Some(Action::AttachTerminal(id.clone(), terminal_mode));
        }
        None
    }

    fn stop_selected(&mut self) {
        if let Some(session_id) = &self.selected_session {
            if let Some(inst) = self.get_instance(session_id) {
                if matches!(
                    inst.status,
                    Status::Stopped | Status::Deleting | Status::Creating
                ) {
                    return;
                }
                let message = format!("Are you sure you want to stop '{}'?", inst.title);
                self.pending_stop_session = Some(session_id.clone());
                self.confirm_dialog =
                    Some(ConfirmDialog::new("Stop Session", &message, "stop_session"));
            }
        }
    }

    fn open_diff_for_selected(&mut self) {
        // Open diff view - requires a selected session.
        let Some(session_id) = &self.selected_session else {
            self.info_dialog = Some(InfoDialog::new(
                "No Session Selected",
                "Select a session to view its diff.",
            ));
            return;
        };

        let Some(inst) = self.get_instance(session_id) else {
            self.info_dialog = Some(InfoDialog::new("Error", "Could not find session data."));
            return;
        };

        let repo_path = std::path::PathBuf::from(&inst.project_path);
        let session_id_owned = inst.id.clone();
        let profile = inst.source_profile.clone();
        let base_override = inst.base_branch_override.clone();
        let worktree_base = inst
            .worktree_info
            .as_ref()
            .and_then(|w| w.base_branch.clone());
        match DiffView::new_for_session(
            repo_path,
            Some(session_id_owned),
            profile,
            base_override,
            worktree_base,
            self.file_watch.clone(),
        ) {
            Ok(view) => self.diff_view = Some(view),
            Err(e) => {
                tracing::error!(target: "tui.input", "Failed to open diff view: {}", e);
                self.info_dialog = Some(InfoDialog::new(
                    "Error",
                    &format!("Failed to open diff view: {}", e),
                ));
            }
        }
    }

    fn open_serve(&mut self) {
        #[cfg(feature = "serve")]
        {
            self.serve_view = Some(crate::tui::dialogs::ServeView::new());
        }
        #[cfg(not(feature = "serve"))]
        {
            self.info_dialog = Some(InfoDialog::new(
                "Serve unavailable",
                "This `aoe` binary was built without the `serve` feature, \
                 so the web dashboard, local network serving, and \
                 Cloudflare Tunnel integration are not included.\n\n\
                 To serve to your phone (LAN / Tailscale / tunnel):\n\
                   \u{2022} Install a release build from GitHub Releases, or\n\
                   \u{2022} Build from source with:\n\
                     cargo build --release --features serve\n\n\
                 Once you have a `serve`-enabled binary, press R again to \
                 open the serve dialog.",
            ));
        }
    }

    pub(super) fn open_settings(&mut self) {
        let project_path = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
            .map(|inst| inst.project_path.clone());
        match SettingsView::new(&self.config_profile(), project_path) {
            Ok(view) => self.settings_view = Some(view),
            Err(e) => {
                tracing::error!(target: "tui.input", "Failed to open settings: {}", e);
                self.info_dialog = Some(InfoDialog::new(
                    "Error",
                    &format!("Failed to open settings: {}", e),
                ));
            }
        }
    }

    fn run_update(&mut self, update_info: Option<&crate::update::UpdateInfo>) -> Option<Action> {
        if let Some(info) = update_info {
            if info.available && self.update_confirm_dialog.is_none() {
                let method = match crate::update::install::detect_install_method() {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::warn!(target: "tui.input", "update detection failed: {e}");
                        return None;
                    }
                };
                use crate::update::install::InstallMethod;
                if !matches!(
                    &method,
                    InstallMethod::Homebrew | InstallMethod::Tarball { .. }
                ) {
                    let msg = match &method {
                        InstallMethod::Nix => {
                            "Nix install: run `nix run github:agent-of-empires/agent-of-empires` to update".to_string()
                        }
                        InstallMethod::Cargo => {
                            "Cargo install: run `cargo install --git https://github.com/agent-of-empires/agent-of-empires aoe`".to_string()
                        }
                        InstallMethod::Unknown { .. } => {
                            "Unknown install method: run `aoe update` in a terminal for instructions".to_string()
                        }
                        _ => unreachable!(),
                    };
                    return Some(Action::SetTransientStatus(msg));
                }
                let needs_sudo = matches!(
                    &method,
                    InstallMethod::Tarball { binary_path }
                        if !crate::update::install::parent_is_writable(binary_path)
                );
                self.update_confirm_dialog = Some(crate::tui::dialogs::UpdateConfirmDialog::new(
                    info.current_version.clone(),
                    info.latest_version.clone(),
                    method,
                    needs_sudo,
                ));
            }
        }
        None
    }

    fn toggle_container_for_selected(&mut self) {
        if let Some(id) = &self.selected_session {
            if let Some(inst) = self.get_instance(id) {
                if inst.is_sandboxed() {
                    let id = id.clone();
                    self.toggle_terminal_mode(&id);
                } else {
                    self.info_dialog = Some(InfoDialog::new(
                        "Not Available",
                        "Only sandboxed sessions support container terminals. This session runs directly on the host.",
                    ));
                }
            }
        }
    }

    fn open_command_palette(&mut self) {
        let serve_enabled = cfg!(feature = "serve");
        let mut entries: Vec<PaletteCommand> = builtin_commands(serve_enabled, self.strict_hotkeys);

        // Quit lives in the registry but is excluded from `builtin_commands`
        // (no palette metadata) so it can sit in the Settings group at the end;
        // add it here, still routed through the shared action dispatch.
        entries.push(PaletteCommand {
            id: bindings::palette_id(ActionId::Quit),
            title: "Quit Agent of Empires".to_string(),
            group: PaletteGroup::Settings,
            keywords: vec!["exit", "close"],
            hotkey: bindings::label(ActionId::Quit, self.strict_hotkeys),
            payload: PaletteAction::Invoke(ActionId::Quit),
        });

        // Dynamic session/group entries: one per flat_items row, so the user
        // can fuzzy-search and jump straight to it. We tag in-flight sessions
        // (Creating / Deleting) in the title so the user knows that picking
        // Stop/Delete from the palette will be a no-op for those rows.
        for (idx, item) in self.flat_items.iter().enumerate() {
            match item {
                Item::Session { id, .. } => {
                    let Some(inst) = self.get_instance(id) else {
                        continue;
                    };
                    let status_tag = match inst.status {
                        Status::Creating => " [creating]",
                        Status::Deleting => " [deleting]",
                        Status::Stopped => " [stopped]",
                        _ => "",
                    };
                    let title = if inst.group_path.is_empty() {
                        format!("Jump to session: {}{}", inst.title, status_tag)
                    } else {
                        format!(
                            "Jump to session: {} ({}){}",
                            inst.title, inst.group_path, status_tag
                        )
                    };
                    entries.push(PaletteCommand {
                        id: "jump-session",
                        title,
                        group: PaletteGroup::Sessions,
                        keywords: vec!["session", "jump", "select"],
                        hotkey: String::new(),
                        payload: PaletteAction::JumpToCursor(idx),
                    });
                }
                Item::Group { name, path, .. } => {
                    // The synthetic Archived section header (and any
                    // sub-folder rendered under it in Project mode) is
                    // not a real group; skip it so the palette doesn't
                    // surface the sentinel path or invite Jump-to-group
                    // navigation that the rest of the codebase
                    // intentionally disarms.
                    if crate::session::is_within_archived_section(path) {
                        continue;
                    }
                    let label = if name == path {
                        format!("Jump to group: {}", name)
                    } else {
                        format!("Jump to group: {} ({})", name, path)
                    };
                    entries.push(PaletteCommand {
                        id: "jump-group",
                        title: label,
                        group: PaletteGroup::Groups,
                        keywords: vec!["group", "jump"],
                        hotkey: String::new(),
                        payload: PaletteAction::JumpToCursor(idx),
                    });
                }
            }
        }

        // Tool session entries, sorted by name for stable palette ordering
        // (matches the tool picker dialog's alphabetical order).
        let mut tools_sorted: Vec<_> = self.tool_configs.iter().collect();
        tools_sorted.sort_by_key(|(name, _)| name.to_owned());
        for (name, config) in tools_sorted {
            let hotkey_label = config
                .hotkey
                .as_deref()
                .map(|h| format!(" [{}]", h))
                .unwrap_or_default();
            entries.push(PaletteCommand {
                id: "tool-session",
                title: format!("Open tool: {}{}", name, hotkey_label),
                group: PaletteGroup::Actions,
                keywords: vec!["tool", "session"],
                hotkey: String::new(),
                payload: PaletteAction::ToolSession(name.clone()),
            });
        }

        self.command_palette = Some(CommandPaletteDialog::new(entries));
    }

    /// Apply a palette pick. `Key` re-enters the action dispatch with the
    /// synthesized event (bypassing strict normalization, which the palette
    /// already accounts for); `JumpToCursor` moves the selection.
    fn dispatch_palette_action(
        &mut self,
        action: PaletteAction,
        update_info: Option<&crate::update::UpdateInfo>,
    ) -> Option<Action> {
        // The palette can now be opened over live mode (via the leader),
        // but every palette command steps out of the per-session relay:
        // jumping navigates away, Invoke/Activate/ToolSession change what's
        // focused, and the preview follows `selected_session` while
        // keystrokes target `live_send`. Committing any of them while still
        // live would desync the preview from the keystroke target, so leave
        // live mode first. Cancelling the palette (Esc) never reaches here,
        // so it still drops the user straight back into live mode.
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
        }
        match action {
            PaletteAction::Invoke(id) => {
                // The palette's mental model is "run the named action," so clear
                // any leftover search-cycle state first: otherwise picking "New
                // session" while a search is active would route the dual-purpose
                // `n`/`N` actions into a search-cycle instead.
                if !self.search_matches.is_empty() {
                    self.search_matches.clear();
                    self.search_match_index = 0;
                }
                self.run_action(id, update_info)
            }
            PaletteAction::Activate => self.activate_selected_session(),
            PaletteAction::LiveSend => self.start_live_send(),
            PaletteAction::JumpToCursor(idx) => {
                if !self.flat_items.is_empty() {
                    self.cursor = idx.min(self.flat_items.len() - 1);
                    self.update_selected();
                }
                None
            }
            PaletteAction::ToolSession(tool_name) => {
                self.view_mode = ViewMode::Tool(tool_name);
                self.preview_scroll_offset = 0;
                self.tool_preview_cache = super::PreviewCache::default();
                None
            }
        }
    }

    fn jump_to_next_waiting(&mut self) {
        let len = self.flat_items.len();
        if len == 0 {
            return;
        }

        // Pass 1: forward-walk from cursor+1, wrapping, for the next Waiting
        // session OR a freshly-stopped Idle session (within
        // `idle_decay_window`). Both states are "needs your attention" and
        // cycle together so repeated `w` taps move through the actionable
        // backlog regardless of which hook fired.
        let window = self.idle_decay_window;
        let start = (self.cursor + 1) % len;
        for i in 0..len - 1 {
            let idx = (start + i) % len;
            let id = match self.flat_items.get(idx) {
                Some(Item::Session { id, .. }) => id.clone(),
                _ => continue,
            };
            if let Some(inst) = self.get_instance(&id) {
                let is_actionable = inst.status == Status::Waiting
                    || matches!(inst.idle_age(), Some(age) if age < window);
                if is_actionable {
                    self.cursor = idx;
                    self.update_selected();
                    return;
                }
            }
        }

        // Pass 2: fall back to the most-recently-accessed Idle session, skipping
        // the cursor. Sessions never attached (last_accessed_at == None) rank
        // last but remain eligible.
        let mut best: Option<(usize, Option<chrono::DateTime<chrono::Utc>>)> = None;
        for idx in 0..len {
            if idx == self.cursor {
                continue;
            }
            let id = match self.flat_items.get(idx) {
                Some(Item::Session { id, .. }) => id.clone(),
                _ => continue,
            };
            let Some(inst) = self.get_instance(&id) else {
                continue;
            };
            if inst.status != Status::Idle {
                continue;
            }
            let ts = inst.last_accessed_at;
            let beats = match best {
                None => true,
                Some((_, b)) => match (ts, b) {
                    (Some(a), Some(b)) => a > b,
                    (Some(_), None) => true,
                    (None, _) => false,
                },
            };
            if beats {
                best = Some((idx, ts));
            }
        }

        if let Some((idx, _)) = best {
            self.cursor = idx;
            self.update_selected();
            return;
        }

        self.info_dialog = Some(InfoDialog::new(
            "No Available Sessions",
            "No sessions are currently waiting or idle.",
        ));
    }

    pub(super) fn move_cursor(&mut self, delta: i32) {
        if self.flat_items.is_empty() {
            return;
        }

        let new_cursor = if delta < 0 {
            self.cursor.saturating_sub((-delta) as usize)
        } else {
            (self.cursor + delta as usize).min(self.flat_items.len() - 1)
        };

        self.cursor = new_cursor;
        // Keyboard nav overrides any prior hover. Without this, when mosh
        // (or any prediction layer) eats the `Moved` event that fires as
        // the cursor leaves the list, the hover background stays painted
        // on the row the mouse was last on while the keyboard-selected
        // row also paints; two highlighted rows at once. handle_hover
        // only clears `mouse_pos` when it RECEIVES an off-list Moved, so
        // any keyboard transition has to clear it directly.
        self.mouse_pos = None;
        self.update_selected();
    }

    /// Resolve the action that "activating" the currently-selected session
    /// should produce (structured view open, attach to tmux session, attach to a
    /// tool session, etc.). Returns `None` for in-flight sessions
    /// (`Creating`/`Deleting`) and when no session is selected. Shared
    /// between the `Enter` keybind and double-click activation so the two
    /// paths can't drift.
    pub(super) fn activate_selected_session(&mut self) -> Option<Action> {
        let id = self.selected_session.clone()?;
        if let Some(inst) = self.get_instance(&id) {
            if matches!(inst.status, Status::Deleting | Status::Creating) {
                return None;
            }
            if inst.is_structured() {
                #[cfg(feature = "serve")]
                {
                    return Some(Action::OpenStructuredView(id));
                }
                #[cfg(not(feature = "serve"))]
                {
                    return Some(Action::SetTransientStatus(
                        "Acp session: rebuild with --features serve to attach".to_string(),
                    ));
                }
            }
        }
        match self.view_mode {
            ViewMode::Structured => {
                // `default_attach_mode = LiveSend` swaps the historical
                // tmux attach for live-send mode on Enter / double-click.
                // Acp was already handled above (the resolver also
                // returns None for structured view, so the match is double-safe);
                // Terminal view honors the same setting (live-send onto
                // the paired terminal pane); Tool view keeps its
                // existing AttachToolSession path.
                //
                // Route through `start_live_send` so the same-target
                // guard (already-live on this session) is honored: a
                // double-click on the live row would otherwise re-run
                // ensure_pane_ready and respawn the worker for no
                // reason. `start_live_send` returns `None` for that
                // and for structured view/creating rows; in either of those
                // cases we leave activation alone (structured view was already
                // dispatched to OpenStructuredView above; same-target re-click
                // is intentionally a no-op).
                if matches!(
                    self.default_attach_mode(&id),
                    Some(crate::session::NewSessionAttachMode::LiveSend)
                ) {
                    self.start_live_send()
                } else {
                    Some(Action::AttachSession(id))
                }
            }
            ViewMode::Terminal => {
                // Mirror Structured view: when `default_attach_mode = LiveSend`,
                // Enter on the terminal row enters live-send mode against
                // the paired terminal pane (host or container, whichever
                // is currently shown). Otherwise fall back to the
                // historical tmux attach.
                if matches!(
                    self.default_attach_mode(&id),
                    Some(crate::session::NewSessionAttachMode::LiveSend)
                ) {
                    return self.start_live_send();
                }
                let terminal_mode = if let Some(inst) = self.get_instance(&id) {
                    if inst.is_sandboxed() {
                        self.get_terminal_mode(&id)
                    } else {
                        TerminalMode::Host
                    }
                } else {
                    TerminalMode::Host
                };
                Some(Action::AttachTerminal(id, terminal_mode))
            }
            ViewMode::Tool(ref tool_name) => Some(Action::AttachToolSession(id, tool_name.clone())),
        }
    }

    /// Resolve the "Tab swap" action that fires when
    /// `default_attach_mode = LiveSend`: Enter takes the live-send
    /// slot, so Tab takes the tmux-attach slot. Mirrors the structured view
    /// and in-flight guards from `activate_selected_session`; returns
    /// the same per-view-mode attach actions Enter produces under the
    /// historical default.
    pub(super) fn tab_attach_action(&mut self) -> Option<Action> {
        let id = self.selected_session.clone()?;
        if let Some(inst) = self.get_instance(&id) {
            if matches!(inst.status, Status::Deleting | Status::Creating) {
                return None;
            }
            if inst.is_structured() {
                #[cfg(feature = "serve")]
                {
                    return Some(Action::OpenStructuredView(id));
                }
                #[cfg(not(feature = "serve"))]
                {
                    return Some(Action::SetTransientStatus(
                        "Acp session: rebuild with --features serve to attach".to_string(),
                    ));
                }
            }
        }
        match self.view_mode {
            ViewMode::Structured => Some(Action::AttachSession(id)),
            ViewMode::Terminal => {
                let terminal_mode = if let Some(inst) = self.get_instance(&id) {
                    if inst.is_sandboxed() {
                        self.get_terminal_mode(&id)
                    } else {
                        TerminalMode::Host
                    }
                } else {
                    TerminalMode::Host
                };
                Some(Action::AttachTerminal(id, terminal_mode))
            }
            ViewMode::Tool(ref tool_name) => Some(Action::AttachToolSession(id, tool_name.clone())),
        }
    }

    pub(super) fn update_selected(&mut self) {
        if let Some(item) = self.flat_items.get(self.cursor) {
            let prev_session = self.selected_session.clone();
            match item {
                Item::Session { id, .. } => {
                    self.selected_session = Some(id.clone());
                    self.selected_group = None;
                    self.selected_group_profile = None;
                }
                Item::Group { path, .. } => {
                    self.selected_session = None;
                    if crate::session::is_within_archived_section(path) {
                        // The synthetic Archived section (and any
                        // project sub-folder rendered under it in
                        // Project mode) is not a real group: it can't
                        // be renamed, deleted, archived, or moved.
                        // Leaving `selected_group` unset disarms every
                        // keybind that branches on
                        // `selected_group.is_some()` (rename, delete,
                        // archive group, etc.) without each one having
                        // to special-case the sentinel.
                        self.selected_group = None;
                        self.selected_group_profile = None;
                    } else {
                        self.selected_group = Some(path.clone());
                        self.selected_group_profile = self.profile_for_cursor(self.cursor);
                    }
                }
            }
            if self.selected_session != prev_session {
                self.preview_scroll_offset = 0;
            }
        }
    }

    /// Put the cursor back on `selected_session` after a `flat_items` rebuild
    /// (sort toggle, group_by toggle). Mode flips reshape the list, especially
    /// when Attention sort is involved, so index-based clamping lands the
    /// cursor on whatever happened to slide into the old slot. Seeking by
    /// session id keeps focus on the row the user was actually looking at.
    /// Falls back to the legacy clamp when there was no prior selection or
    /// the session is no longer in the flat list (e.g., collapsed under a
    /// group header).
    pub(super) fn reseat_cursor_after_rebuild(&mut self) {
        if let Some(sid) = self.selected_session.clone() {
            for (idx, item) in self.flat_items.iter().enumerate() {
                if let Item::Session { id, .. } = item {
                    if *id == sid {
                        self.cursor = idx;
                        self.update_selected();
                        return;
                    }
                }
            }
        }
        self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
        self.update_selected();
    }

    fn apply_sort_order(&mut self, new_order: SortOrder) {
        self.sort_order = new_order;
        self.flat_items = self.build_flat_items();
        if self.search_active && !self.search_query.value().is_empty() {
            self.update_search();
        } else {
            self.reseat_cursor_after_rebuild();
        }
        if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
            config.app_state.sort_order = Some(self.sort_order);
            if let Err(e) = save_config(&config) {
                tracing::warn!(target: "tui.input", "Failed to save sort order: {}", e);
            }
        }
    }

    fn apply_group_by(&mut self, new_mode: GroupByMode) {
        self.group_by = new_mode;
        self.flat_items = self.build_flat_items();
        self.reseat_cursor_after_rebuild();
        match load_config().map(|c| c.unwrap_or_default()) {
            Ok(mut config) => {
                config.app_state.group_by = Some(self.group_by);
                if let Err(e) = save_config(&config) {
                    tracing::warn!(target: "tui.input", "Failed to save group_by mode: {}", e);
                }
            }
            Err(e) => {
                tracing::warn!(target: "tui.input", "Failed to load config for group_by save: {}", e);
            }
        }
    }

    fn toggle_group_collapsed(&mut self, path: &str) {
        // The synthetic Archived section is not a member of any
        // GroupTree; its collapsed state lives on HomeView and persists
        // separately. Route here before either branch tries to mutate a
        // nonexistent group.
        if crate::session::is_archived_section_path(path) {
            self.toggle_archived_section();
            return;
        }
        if self.group_by == GroupByMode::Project {
            let collapsed = self
                .project_group_collapsed
                .get(path)
                .copied()
                .unwrap_or(false);
            self.project_group_collapsed
                .insert(path.to_string(), !collapsed);
            self.flat_items = self.build_flat_items();
            return;
        }
        // Route to the correct profile's GroupTree
        let profile = self.profile_for_cursor(self.cursor);
        if let Some(profile) = profile {
            if let Some(tree) = self.group_trees.get_mut(&profile) {
                tree.toggle_collapsed(path);
            }
        }
        self.flat_items = self.build_flat_items();
        if let Err(e) = self.save() {
            tracing::error!(target: "tui.input", "Failed to save group state: {}", e);
        }
    }

    /// Route a mouse-wheel-up at (col, row) to the pane under the cursor:
    /// diff view (if open) → diff scroll; list pane → list cursor up;
    /// preview pane → preview scroll. Returns `true` if the UI should
    /// redraw. Scrolls do not cross pane boundaries: a wheel over the
    /// preview never moves the list cursor, even when the preview is at
    /// its scroll boundary or has no session selected.
    pub fn handle_scroll_up(&mut self, col: u16, row: u16) -> bool {
        const STEP: u16 = 3;
        // A preview selection is anchored to absolute scrollback lines,
        // not screen cells, so scrolling no longer invalidates it: the
        // highlight tracks its text as the pane moves and the copy spans
        // the full range even where it has scrolled off screen. So we
        // deliberately do NOT clear it here.
        if let Some(ref mut diff) = self.diff_view {
            diff.scroll_up(STEP);
            return true;
        }
        // Live-send mode lets the user scroll the preview to read
        // agent history without exiting, but list scroll is suppressed
        // (changing the selection mid-live-send would silently aim the
        // next keystroke at a different pane than the one the user is
        // looking at). All other modals swallow scroll entirely.
        if self.live_send.is_some() {
            if !self.hit_preview(col, row) {
                return false;
            }
        } else {
            if self.has_dialog() {
                return false;
            }
            if self.hit_list(col, row) {
                self.move_cursor(-1);
                return true;
            }
            if !self.hit_preview(col, row) {
                return false;
            }
        }
        if self.selected_session.is_none() {
            return false;
        }

        let active_cache = match self.view_mode {
            ViewMode::Structured => &self.preview_cache,
            ViewMode::Terminal => {
                let terminal_mode = self
                    .selected_session
                    .as_ref()
                    .and_then(|id| self.get_instance(id))
                    .map(|inst| {
                        if inst.is_sandboxed() {
                            self.get_terminal_mode(&inst.id)
                        } else {
                            TerminalMode::Host
                        }
                    })
                    .unwrap_or(TerminalMode::Host);
                match terminal_mode {
                    TerminalMode::Container => &self.container_terminal_preview_cache,
                    TerminalMode::Host => &self.terminal_preview_cache,
                }
            }
            ViewMode::Tool(_) => &self.tool_preview_cache,
        };

        let visible_height = active_cache.dimensions.1.saturating_sub(1) as usize;
        let real_max = active_cache.captured_lines.saturating_sub(visible_height) as u16;

        let new_offset = self.preview_scroll_offset.saturating_add(STEP);
        let clamped = new_offset.min(real_max);
        if clamped == self.preview_scroll_offset {
            return false;
        }
        self.preview_scroll_offset = clamped;
        true
    }

    /// Map a (col, row) inside the list's inner content rect to a
    /// `flat_items` index, or `None` for rows that don't resolve to a real
    /// item (search bar, `[N more above/below]` indicator rows, empty list,
    /// outside the inner rect, dialog open, diff view active). Shared by
    /// `handle_click` and `hovered_index` so selection and hover use the
    /// exact same math.
    ///
    /// Live-send is intentionally NOT treated as a blocking dialog here.
    /// `has_dialog()` returns true while live mode is active so other
    /// surfaces (key shortcuts, preview-click, scroll wheel) stay
    /// frozen, but clicks on list rows are how the user switches the
    /// live target session: blocking them would make the feature
    /// unreachable via mouse.
    pub(super) fn resolve_row_to_index(&self, col: u16, row: u16) -> Option<usize> {
        if self.diff_view.is_some() || self.has_non_live_send_overlay() {
            return None;
        }
        let inner = self.list_inner_area;
        if !inner.contains(Position::from((col, row))) {
            return None;
        }
        if self.flat_items.is_empty() {
            return None;
        }
        let visible_height = if self.search_active {
            (inner.height as usize).saturating_sub(1)
        } else {
            inner.height as usize
        };
        if visible_height == 0 {
            return None;
        }
        let row_in_inner = row.saturating_sub(inner.y) as usize;
        if self.search_active && row_in_inner + 1 == inner.height as usize {
            return None;
        }

        let scroll = crate::tui::components::scroll::calculate_scroll(
            self.flat_items.len(),
            self.cursor,
            visible_height,
        );
        let row_offset = if scroll.has_more_above { 1 } else { 0 };
        if row_in_inner < row_offset {
            return None;
        }
        let item_row = row_in_inner - row_offset;
        if item_row >= scroll.list_visible {
            return None;
        }
        let abs_idx = scroll.scroll_offset + item_row;
        if abs_idx >= self.flat_items.len() {
            return None;
        }
        Some(abs_idx)
    }

    /// Currently hovered `flat_items` index, derived from the last mouse
    /// position. `None` when the mouse is off the list or over a row that
    /// doesn't resolve to a real item. Recomputed on every call so wheel
    /// scrolls implicitly move the hover with the items under the cursor.
    pub(super) fn hovered_index(&self) -> Option<usize> {
        self.mouse_pos
            .and_then(|(c, r)| self.resolve_row_to_index(c, r))
    }

    /// Handle a right-click at `(col, row)`. When it lands on a sidebar
    /// row, move the cursor onto that row (so Rename/Delete target what
    /// the user actually clicked, not whatever was selected before) and
    /// open the context menu anchored to the click position. The
    /// renderer clamps the menu into the visible area so a near-edge
    /// click never produces an off-screen popup.
    ///
    /// Returns true when a menu opened. Routes by what the click hit:
    /// a real row (session or group) opens the per-row Rename/Delete
    /// menu; empty space inside the list opens the empty-sidebar menu
    /// (New Session / Change Sort / Change Grouping). Anywhere else
    /// (header, scroll arrow, scroll bar, outside the list panel) is
    /// a no-op so the caller can fall through.
    pub fn handle_right_click(&mut self, col: u16, row: u16) -> bool {
        // `resolve_row_to_index` already short-circuits when any
        // non-live-send overlay (incl. the context menu itself) is open
        // and inside the diff takeover, so no extra dialog gating here.
        let anchor = (col.saturating_add(1), row.saturating_add(1));
        if let Some(idx) = self.resolve_row_to_index(col, row) {
            if self.cursor != idx {
                self.cursor = idx;
                self.update_selected();
            }
            // Mirror the row-aware menu copy from the web sidebar so a group
            // row reads as "Rename Group / Delete Group" instead of bare
            // "Rename / Delete".
            let is_group = matches!(self.flat_items[idx], super::Item::Group { .. });
            // A real project header in project view gets the pin menu; the
            // cursor was just moved onto this row, so `project_group_at_cursor`
            // reflects it. Manual/synthetic group rows keep Rename/Delete.
            let project_label = self.project_group_at_cursor();
            self.context_menu = Some(if let Some(label) = project_label {
                ContextMenuDialog::for_project_group(anchor, self.is_project_label_pinned(&label))
            } else if is_group {
                ContextMenuDialog::for_group(anchor)
            } else {
                let (is_archived, is_snoozed) = match &self.flat_items[idx] {
                    super::Item::Session { id, .. } => self
                        .get_instance(id)
                        .map(|inst| (inst.is_archived(), inst.is_snoozed()))
                        .unwrap_or((false, false)),
                    super::Item::Group { .. } => (false, false),
                };
                // Snooze is an Attention-sort triage primitive: the `'h'`
                // keybinding only fires in Attention sort, so the menu omits
                // the Snooze row everywhere else to keep the mouse and keyboard
                // paths in step.
                let snooze = (self.sort_order == crate::session::config::SortOrder::Attention)
                    .then_some(is_snoozed);
                ContextMenuDialog::for_session(anchor, is_archived, snooze)
            });
            return true;
        }
        // No row resolved. If the click landed inside the list panel
        // anyway (empty space below the last session, or an empty
        // list), surface the empty-sidebar menu so the mouse-only path
        // can reach the n/o/g entry points. Gated on no other overlay
        // being open and the diff view not taking over the panel, same
        // shape as `handle_empty_list_click`.
        if self.has_non_live_send_overlay() || self.diff_view.is_some() {
            return false;
        }
        if !self.list_inner_area.contains(Position::from((col, row))) {
            return false;
        }
        self.context_menu = Some(ContextMenuDialog::for_empty_sidebar(anchor));
        true
    }

    /// Route a left-click into the context menu, if it's open. Three
    /// outcomes from the menu's perspective:
    ///   - click on a Rename / Delete row: dispatch the action (which
    ///     opens the matching follow-up dialog) and close the menu,
    ///   - click on the menu's border (or anywhere inside that isn't a
    ///     row): keep it open,
    ///   - click outside the menu: close it.
    ///
    /// In all three cases the click is "consumed"; the caller must not
    /// fall through to the list / preview / dialog handlers underneath.
    /// Returns true when the menu existed (and consumed the click).
    pub fn handle_context_menu_click(&mut self, col: u16, row: u16) -> bool {
        let Some(menu) = &mut self.context_menu else {
            return false;
        };
        match menu.handle_click(col, row) {
            None => {
                self.context_menu = None;
            }
            Some(DialogResult::Continue) => {
                // Inside the menu but not on a row (border): keep open.
            }
            Some(DialogResult::Cancel) => {
                self.context_menu = None;
            }
            Some(DialogResult::Submit(action)) => {
                self.context_menu = None;
                self.dispatch_context_menu_action(action);
            }
        }
        true
    }

    /// Single dispatcher for every `ContextMenuAction` so the keyboard
    /// path (Enter / r / d / n / o / g on an open menu) and the mouse
    /// path (click on a menu row) execute the exact same helpers. Any
    /// new menu action needs to be wired here once, not at each call
    /// site.
    pub(super) fn dispatch_context_menu_action(&mut self, action: ContextMenuAction) {
        match action {
            ContextMenuAction::Rename => self.open_rename_for_selected(),
            ContextMenuAction::Delete => self.open_delete_for_selected(),
            ContextMenuAction::ToggleArchive => {
                // The right-click already moved the cursor onto the row, so the
                // toggle acts on the same session the menu was opened for.
                if let Err(e) = self.toggle_archive_at_cursor() {
                    tracing::error!("toggle_archive_at_cursor (context menu) failed: {}", e);
                }
            }
            ContextMenuAction::ToggleSnooze => {
                // Same cursor-on-the-clicked-row guarantee as ToggleArchive:
                // snoozing an active row opens the duration picker, unsnoozing
                // wakes it immediately.
                if let Err(e) = self.toggle_snooze_at_cursor() {
                    tracing::error!("toggle_snooze_at_cursor (context menu) failed: {}", e);
                }
            }
            ContextMenuAction::NewSession => self.open_new_session_dialog(),
            // The right-click already moved the cursor onto the group row, so
            // reuse the "new from selection" path: with a group selected it
            // prefills the project's repo path (and group) the same way `'N'`
            // does on a session.
            ContextMenuAction::NewFromGroup => self.open_new_from_selection(),
            ContextMenuAction::OpenSortPicker => self.show_sort_picker(),
            ContextMenuAction::OpenGroupPicker => self.show_group_picker(),
            ContextMenuAction::TogglePin => {
                // The right-click already moved the cursor onto the project
                // header, so the toggle acts on the same project the menu was
                // opened for.
                self.toggle_project_pin_at_cursor();
            }
        }
    }

    /// Open the new-session dialog, with the same gating the `'n'` key
    /// applies: a "please wait" info dialog when a session is already
    /// being created, the no-agents dialog when no tool is available,
    /// otherwise the full dialog. Shared by `'n'` and the
    /// click-in-empty-sidebar shortcut so they can't drift.
    pub(super) fn open_new_session_dialog(&mut self) {
        if self.creating_stub_id.is_some() {
            self.info_dialog = Some(InfoDialog::new(
                "Please Wait",
                "A session is already being created. Wait for it to finish or press Ctrl+C to cancel.",
            ));
            return;
        }
        if !self.available_tools.any_available() {
            self.show_no_agents();
            return;
        }
        let existing_groups: Vec<String> =
            self.all_groups().iter().map(|g| g.path.clone()).collect();
        let current_profile = self.config_profile();
        let profiles = list_profiles().unwrap_or_else(|_| vec![current_profile.clone()]);
        self.new_dialog = Some(NewSessionDialog::new(
            self.available_tools.clone(),
            existing_groups,
            &current_profile,
            profiles,
        ));
    }

    /// Left-click on the empty area of the sidebar (below the last
    /// session, or in an empty list). Used as a quick "drop out of
    /// live mode" gesture: if live-send is active, the click exits
    /// it and reflows the preview back to its normal size; otherwise
    /// the click is a no-op. The "open new session" entry that used
    /// to live here now belongs to the right-click empty-sidebar
    /// menu, so left-click on empty space stays low-stakes and the
    /// user never accidentally summons a modal mid-typing.
    ///
    /// Gated to fire only when no overlay is already up and the diff
    /// view isn't open, so a click on an empty list while a modal is
    /// covering it doesn't punch through.
    pub fn handle_empty_list_click(&mut self, col: u16, row: u16) -> bool {
        if self.has_non_live_send_overlay() || self.diff_view.is_some() {
            return false;
        }
        if !self.list_inner_area.contains(Position::from((col, row))) {
            return false;
        }
        if self.resolve_row_to_index(col, row).is_some() {
            // A real row resolved here; the regular click path owns it.
            return false;
        }
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
            return true;
        }
        false
    }

    /// Open the rename dialog for whatever the sidebar has selected (a
    /// session row, or a manual-mode group). Project-mode groups can't be
    /// renamed, so they raise an info dialog explaining how to switch
    /// modes. No-op when nothing is selected, or when the selected session
    /// is mid-create or mid-delete (renaming under those states would race
    /// the cascade).
    ///
    /// Shared by the `'r'` / `'R'` key handlers and the right-click
    /// context menu so all three entry points stay byte-identical.
    pub(super) fn open_rename_for_selected(&mut self) {
        if let Some(id) = &self.selected_session {
            if let Some(inst) = self.get_instance(id) {
                if matches!(inst.status, Status::Deleting | Status::Creating) {
                    return;
                }
                // Rename is anchored to the selected session, so the dialog
                // must open against that session's profile, not the
                // view-level active/config profile (which can differ in
                // all-profiles mode).
                let current_profile = inst.source_profile.clone();
                let profiles = list_profiles().unwrap_or_else(|_| vec![current_profile.clone()]);
                let existing_groups: Vec<String> =
                    self.all_groups().iter().map(|g| g.path.clone()).collect();
                self.rename_dialog = Some(RenameDialog::new(
                    &inst.title,
                    &inst.group_path,
                    &current_profile,
                    profiles,
                    existing_groups,
                ));
            }
        } else if let Some(group_path) = &self.selected_group {
            if self.group_by == GroupByMode::Project {
                let hint = if self.strict_hotkeys {
                    "Project groups are automatic. Press Ctrl+G and pick Manual to manage groups."
                } else {
                    "Project groups are automatic. Press 'g' and pick Manual to manage groups."
                };
                self.info_dialog = Some(InfoDialog::new("Cannot Modify Project Groups", hint));
                return;
            }
            let group_path = group_path.clone();
            let current_profile = self
                .selected_group_profile
                .clone()
                .unwrap_or_else(|| self.config_profile());
            let profiles = list_profiles().unwrap_or_else(|_| vec![current_profile.clone()]);
            let existing_groups: Vec<String> =
                self.all_groups().iter().map(|g| g.path.clone()).collect();
            self.group_rename_context = Some(super::GroupRenameContext {
                old_path: group_path.clone(),
                old_profile: current_profile.clone(),
            });
            self.rename_dialog = Some(RenameDialog::new_for_group(
                &group_path,
                &current_profile,
                profiles,
                existing_groups,
            ));
        }
    }

    /// Open the edit-workdir-name dialog for the selected session. Only
    /// valid for an aoe-managed worktree session that is not running; other
    /// cases surface an info dialog explaining why.
    pub(super) fn open_worktree_name_for_selected(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        // Tied mode (#1927) collapses naming into a single Rename action: the
        // directory follows the title, so route the standalone workdir edit to
        // the rename dialog instead of editing the directory independently.
        if self.tie_workdir_applies_for(&id) {
            self.open_rename_for_selected();
            return;
        }
        let snapshot = self.get_instance(&id).map(|inst| {
            (
                inst.worktree_info.clone(),
                inst.status,
                inst.project_path.clone(),
            )
        });
        let Some((worktree_info, status, project_path)) = snapshot else {
            return;
        };
        let Some(wt) = worktree_info else {
            self.info_dialog = Some(InfoDialog::new(
                "Not a Worktree Session",
                "This session does not use a worktree, so it has no workdir name to edit.",
            ));
            return;
        };
        if !wt.managed_by_aoe {
            self.info_dialog = Some(InfoDialog::new(
                "Worktree Not Managed by AoE",
                "This worktree was attached rather than created by AoE, so its workdir name cannot be edited.",
            ));
            return;
        }
        if status.blocks_worktree_edit() {
            self.info_dialog = Some(InfoDialog::new(
                "Session Active",
                "Stop the session before editing its workdir name.",
            ));
            return;
        }
        let current_dir = std::path::Path::new(&project_path)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&project_path)
            .to_string();
        self.worktree_name_dialog = Some(WorktreeNameDialog::new(&current_dir, &wt.branch));
    }

    /// Open the delete dialog (or a force-remove confirm, or a group
    /// delete-options dialog) for the sidebar's current selection. Mirrors
    /// the gating of the historical `'d'` / `'D'` key handlers:
    ///   - Terminal view rejects deletion with an info dialog,
    ///   - Creating sessions are inert,
    ///   - Stuck-Deleting sessions get a force-remove confirm,
    ///   - Project-mode groups can't be deleted (info dialog).
    ///
    /// Shared by the `'d'` / `'D'` key handlers and the right-click
    /// context menu.
    pub(super) fn open_delete_for_selected(&mut self) {
        // Deletion only allowed in Structured View.
        if self.view_mode == ViewMode::Terminal {
            let hint = if self.strict_hotkeys {
                "Terminals cannot be deleted directly. Switch to Structured View (press Shift+T) and delete the agent session instead."
            } else {
                "Terminals cannot be deleted directly. Switch to Structured View (press 't') and delete the agent session instead."
            };
            self.info_dialog = Some(InfoDialog::new("Cannot Delete Terminal", hint));
            return;
        }
        if let Some(session_id) = &self.selected_session {
            if let Some(inst) = self.get_instance(session_id) {
                if inst.status == Status::Creating {
                    return;
                }
                if inst.status == Status::Deleting {
                    let message = format!(
                        "'{}' is stuck deleting. Force remove it from the session list? \
                         (worktrees, branches, and containers will not be cleaned up)",
                        inst.title
                    );
                    self.pending_force_remove_session = Some(session_id.clone());
                    self.confirm_dialog = Some(ConfirmDialog::new(
                        "Force Remove",
                        &message,
                        "force_remove_session",
                    ));
                    return;
                }

                let config = DeleteDialogConfig {
                    worktree_branch: inst
                        .worktree_info
                        .as_ref()
                        .filter(|wt| wt.managed_by_aoe)
                        .map(|wt| wt.branch.clone())
                        .or_else(|| inst.workspace_info.as_ref().map(|w| w.branch.clone())),
                    has_sandbox: inst.sandbox_info.as_ref().is_some_and(|s| s.enabled),
                    project_path: Some(inst.project_path.clone()),
                    is_scratch: inst.scratch,
                };

                let profile = self.config_profile();
                self.unified_delete_dialog = Some(UnifiedDeleteDialog::new(
                    inst.title.clone(),
                    config,
                    &profile,
                ));
            } else {
                let profile = self.config_profile();
                self.unified_delete_dialog = Some(UnifiedDeleteDialog::new(
                    "Unknown Session".to_string(),
                    DeleteDialogConfig::default(),
                    &profile,
                ));
            }
        } else if let Some(group_path) = &self.selected_group {
            if self.group_by == GroupByMode::Project {
                let hint = if self.strict_hotkeys {
                    "Project groups are automatic. Press Ctrl+G and pick Manual to manage groups."
                } else {
                    "Project groups are automatic. Press 'g' and pick Manual to manage groups."
                };
                self.info_dialog = Some(InfoDialog::new("Cannot Modify Project Groups", hint));
                return;
            }
            let prefix = format!("{}/", group_path);
            let session_count = self
                .instances
                .iter()
                .filter(|i| i.group_path == *group_path || i.group_path.starts_with(&prefix))
                .count();

            if session_count > 0 {
                let has_managed_worktrees = self.group_has_managed_worktrees(group_path, &prefix);
                let has_containers = self.group_has_containers(group_path, &prefix);
                self.group_delete_options_dialog = Some(GroupDeleteOptionsDialog::new(
                    group_path.clone(),
                    session_count,
                    has_managed_worktrees,
                    has_containers,
                ));
            } else {
                let message = format!("Are you sure you want to delete group '{}'?", group_path);
                self.confirm_dialog =
                    Some(ConfirmDialog::new("Delete Group", &message, "delete_group"));
            }
        }
    }

    /// Route a left-click at (col, row) inside the session list. A
    /// single click on a session row selects it AND requests live-send
    /// mode for that row (same `Action::EnterLiveSend` that Tab would
    /// emit); a single click on a group row toggles its collapsed
    /// state; a second click on the same session row within
    /// `DOUBLE_CLICK_THRESHOLD` activates the session (the same Action
    /// the `Enter` keybind would have produced) so users can still
    /// drop into a full tmux attach without going through live mode.
    /// Returns the action for the caller to dispatch, or `None` for
    /// no-op clicks (group toggle, structured view/creating rows, same-session
    /// re-clicks while already live). The caller redraws unconditionally
    /// so the moved cursor / toggled group always paints before the
    /// action executes. Gated by `has_dialog()` (via
    /// `resolve_row_to_index`) so clicks don't shift selection out
    /// from under an open modal.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<Action> {
        self.handle_click_at(std::time::Instant::now(), col, row)
    }

    /// Same as `handle_click`, but the caller supplies `now`. Used by
    /// unit tests to drive double-click detection deterministically
    /// without relying on `thread::sleep`.
    pub(super) fn handle_click_at(
        &mut self,
        now: std::time::Instant,
        col: u16,
        row: u16,
    ) -> Option<Action> {
        let abs_idx = self.resolve_row_to_index(col, row)?;

        let is_double_click = matches!(
            self.last_click,
            Some((prev_time, _, prev_row))
                if prev_row == row
                    && now.duration_since(prev_time) <= DOUBLE_CLICK_THRESHOLD
        );
        self.last_click = Some((now, col, row));

        let item = self.flat_items[abs_idx].clone();
        if is_double_click {
            // First click already selected the row (and toggled a group);
            // the second click only activates a session. Re-toggling a
            // group on the second click would undo the first toggle and
            // flicker, so groups intentionally swallow the second click.
            //
            // We re-sync `cursor` to `abs_idx` before activating because
            // anything between the two clicks (an arrow keypress, a
            // status-poll-driven re-sort) can move the cursor away from
            // the row the user is actually double-clicking. Without this,
            // `activate_selected_session()` reads `selected_session` —
            // which tracks `cursor`, not the click target; and we'd open
            // the wrong session.
            return match item {
                Item::Session { .. } => {
                    if self.cursor != abs_idx {
                        self.cursor = abs_idx;
                        self.update_selected();
                    }
                    self.activate_selected_session()
                }
                Item::Group { .. } => None,
            };
        }

        match item {
            Item::Group { path, .. } => {
                self.toggle_group_collapsed(&path);
                None
            }
            Item::Session { id, .. } => {
                if self.cursor != abs_idx {
                    self.cursor = abs_idx;
                    self.update_selected();
                }
                // An archived row is parked: its pane was killed on archive.
                // A single click is a "let me look at this" gesture, so it
                // must NOT enter live-send, because `start_live_send` would
                // respawn the pane (ensure_pane_ready) and the live-send path
                // would auto-unarchive it (touch_last_accessed), silently
                // resurrecting a session the user deliberately parked. Stop at
                // the cursor update so the row just gets selected. Bringing it
                // back stays explicit: `z` to unarchive, or a deliberate
                // double-click / Enter to open it.
                let archived = self
                    .get_instance(&id)
                    .map(|inst| inst.is_archived())
                    .unwrap_or(false);
                // Single-click behavior is otherwise user-configurable via
                // `SessionConfig::click_action`. `LiveSend` (default,
                // historical behavior) enters live-send for the clicked
                // row, or switches the live target when already in live
                // mode. `SelectOnly` stops at the cursor update above so
                // the user can browse preview content without ever
                // entering live-send (and, if a *different* row was already
                // live, exits live mode so keystrokes don't stay aimed at the
                // old session); double-click still activates via
                // `default_attach_mode`. `click_action` returns `None`
                // for structured view-mode sessions, where `start_live_send`
                // already short-circuits, so the historical fall-through
                // is fine.
                if archived
                    || matches!(
                        self.click_action(&id),
                        Some(crate::session::ClickAction::SelectOnly)
                    )
                {
                    // The click only moves the cursor, but if we're live-sending
                    // to a *different* row, leave live mode rather than stranding
                    // keystrokes on the old session while the cursor / preview
                    // walks away. In `LiveSend` mode the `start_live_send` branch
                    // below already retargets, so this only matters for the
                    // select-only (and archived) gesture, which is precisely a
                    // "stop touching that, let me look at this" intent.
                    if let Some(state) = self
                        .live_send
                        .as_ref()
                        .filter(|s| s.session_id != id)
                        .cloned()
                    {
                        self.exit_live_send_and_restore_sizing(&state);
                    }
                    None
                } else {
                    self.start_live_send()
                }
            }
        }
    }

    /// Record the mouse position from a `MouseEventKind::Moved` event so
    /// the list can render a hover highlight on the row under the cursor.
    /// `mouse_pos` is cleared when the cursor leaves `list_inner_area`.
    /// Returns `true` only when the resolved hovered item changes, so the
    /// caller can skip a redraw on every pixel-level mouse twitch.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        // Open overlay dialogs / menus get hover routed first so their
        // focus highlight tracks the mouse the same way a desktop UI
        // would. The sidebar's own hover state still updates underneath
        // so the row highlight is correct the instant the dialog closes.
        let mut overlay_changed = false;
        if let Some(menu) = &mut self.context_menu {
            overlay_changed |= menu.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.unified_delete_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.new_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(view) = &mut self.settings_view {
            overlay_changed |= view.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.confirm_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.update_confirm_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.telemetry_consent_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.snooze_duration_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.no_agents_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.repo_trust_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(picker) = &mut self.tool_picker_dialog {
            overlay_changed |= picker.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.group_delete_options_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.rename_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.restart_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.hooks_install_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.sort_picker_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.group_picker_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.project_session_picker_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(palette) = &mut self.command_palette {
            overlay_changed |= palette.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.intro_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.info_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }
        if let Some(dialog) = &mut self.changelog_dialog {
            overlay_changed |= dialog.handle_hover(col, row);
        }

        let new_pos = if self.list_inner_area.contains(Position::from((col, row))) {
            Some((col, row))
        } else {
            None
        };
        let prev_idx = self.hovered_index();
        self.mouse_pos = new_pos;
        let new_idx = self.hovered_index();
        overlay_changed || prev_idx != new_idx
    }

    /// Route a mouse-wheel-down at (col, row); see handle_scroll_up.
    pub fn handle_scroll_down(&mut self, col: u16, row: u16) -> bool {
        const STEP: u16 = 3;
        // Mirror handle_scroll_up: the selection is anchored to scrollback
        // lines, so it survives the scroll and is left in place.
        if let Some(ref mut diff) = self.diff_view {
            diff.scroll_down(STEP);
            return true;
        }
        // See handle_scroll_up for the live-send / has_dialog reasoning.
        if self.live_send.is_some() {
            if !self.hit_preview(col, row) {
                return false;
            }
        } else {
            if self.has_dialog() {
                return false;
            }
            if self.hit_list(col, row) {
                self.move_cursor(1);
                return true;
            }
            if !self.hit_preview(col, row) {
                return false;
            }
        }
        if self.selected_session.is_none() {
            return false;
        }
        if self.preview_scroll_offset == 0 {
            return false;
        }
        self.preview_scroll_offset = self.preview_scroll_offset.saturating_sub(STEP);
        true
    }

    /// Route a bracketed paste event to the active text input dialog.
    ///
    /// Live-send mode wins above every dialog: a paste while the user is
    /// "attached" should stream straight to the agent's pane, not buffer
    /// in a dialog the user isn't even looking at. Text-input dialogs
    /// (rename / send_message / new) come next so multi-line dictation
    /// lands in whichever dialog the user is actively typing into. The
    /// settings view is checked last; its paste handler strips newlines,
    /// which would destroy multi-line dictation if we checked it first.
    pub fn handle_paste(&mut self, text: &str) {
        if let Some(state) = self.live_send.clone() {
            // Mirror the live-send key path: any interaction dismisses
            // the finalized highlight so it doesn't follow agent output
            // through subsequent renders.
            self.clear_preview_selection();
            if let Some(worker) = &self.live_send_worker {
                for key in split_paste_for_live_send(text) {
                    worker.send(key);
                }
            }
            self.stamp_last_accessed(&state.session_id);
            return;
        }
        if let Some(ref mut dialog) = self.rename_dialog {
            dialog.handle_paste(text);
            return;
        }
        if let Some(ref mut dialog) = self.worktree_name_dialog {
            dialog.handle_paste(text);
            return;
        }
        if let Some(ref mut dialog) = self.send_message_dialog {
            dialog.handle_paste(text);
            return;
        }
        if let Some(ref mut dialog) = self.new_dialog {
            dialog.handle_paste(text);
            return;
        }
        if let Some(ref mut settings) = self.settings_view {
            settings.handle_paste(text);
            return;
        }

        // No dialog open: route the paste into a new compose dialog if the
        // selected session is runnable. If not, stash in pending_paste so the
        // next dialog open (typically the next `m` press) drains it. Never
        // throw voice text on the floor; losing dictation is worse than
        // silently catching it.
        if let Some((id, title, target)) = self.resolve_send_target() {
            let label = live_send::format_target_label(&title, target);
            self.pending_send_session = Some(id);
            self.pending_send_target = target;
            let mut dialog = SendMessageDialog::new(&label);
            dialog.handle_paste(text);
            self.send_message_dialog = Some(dialog);
            return;
        }

        // No running sessions at all (or all Creating). Stash for later;
        // the user will see the text on next 'm' / dialog open.
        match self.pending_paste.as_mut() {
            Some(buf) => buf.push_str(text),
            None => self.pending_paste = Some(text.to_string()),
        }
    }

    /// Open the restart dialog for the currently-selected session. The dialog
    /// pre-fills profile + AI engine from the instance's current values, and on
    /// submit restarts the session, optionally migrating to the picked profile
    /// and/or swapping the AI engine. No-op if no session is selected or the
    /// selected session is mid-transition.
    fn open_restart_dialog(&mut self) {
        // Match the new-session paths: bail with the no-agents modal if no
        // tool is installed, instead of opening a picker with an empty
        // tool list the user would have to submit blank.
        if !self.available_tools.any_available() {
            self.show_no_agents();
            return;
        }
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(inst) = self.get_instance(&id) else {
            return;
        };
        if matches!(inst.status, Status::Deleting | Status::Creating) {
            return;
        }
        let current_title = inst.title.clone();
        let current_profile = if inst.source_profile.is_empty() {
            self.active_profile
                .clone()
                .unwrap_or_else(|| "default".to_string())
        } else {
            inst.source_profile.clone()
        };
        let current_tool = inst.tool.clone();
        let current_command = inst.command.clone();
        let current_extra_args = inst.extra_args.clone();
        let profiles = list_profiles().unwrap_or_else(|_| vec![current_profile.clone()]);
        let tools: Vec<String> = self.available_tools.available_list().to_vec();
        self.restart_dialog = Some(RestartDialog::new(
            &current_title,
            &current_profile,
            &current_tool,
            &current_command,
            &current_extra_args,
            profiles,
            tools,
        ));
    }

    /// Attempt to enter live-send mode against the currently-selected
    /// session. Unlike `resolve_send_target`, this does NOT require
    /// the tmux pane to already exist: `prepare_live_send` calls
    /// `ensure_pane_ready` which revives stopped sessions (Docker
    /// start, splash wait, resume cascade). Without this relaxation
    /// Tab would silently no-op on dead-but-recoverable rows and the
    /// "Reviving..." toast plumbing would never fire.
    ///
    /// Still no-ops on group headers, empty lists, and Creating rows
    /// (no instance yet, nothing to revive). Also no-ops when the
    /// selected session is already the live-send target so click-to-
    /// enter doesn't re-run ensure_pane_ready / drop the live worker
    /// when the user clicks the same row twice.
    pub(super) fn start_live_send(&mut self) -> Option<Action> {
        let id = self.selected_session.clone()?;
        if self.live_send.as_ref().is_some_and(|s| s.session_id == id) {
            return None;
        }
        let inst = self.get_instance(&id)?;
        if matches!(inst.status, Status::Creating | Status::Deleting) {
            return None;
        }
        // Acp-mode sessions are not tmux-backed (HomeView's attach
        // path special-cases them away from tmux). Live-send has no
        // target in that mode, so silently no-op rather than enqueue
        // an Action::EnterLiveSend that would fail downstream.
        if inst.is_structured() {
            return None;
        }
        // Pick the live-send target based on which pane the user is
        // currently previewing. Structured view → agent pane (historical
        // default). Terminal view → the paired host or container
        // terminal pane, so 'm'/Tab compose against the same shell
        // the user sees. Tool view stays out of live-send (no clean
        // target for lazygit/yazi etc.; let the caller fall back to
        // AttachToolSession).
        self.pending_live_send_target = match &self.view_mode {
            ViewMode::Structured => live_send::LiveSendTarget::Agent,
            ViewMode::Terminal => {
                if inst.is_sandboxed() && self.get_terminal_mode(&id) == TerminalMode::Container {
                    live_send::LiveSendTarget::ContainerTerminal
                } else {
                    live_send::LiveSendTarget::Terminal
                }
            }
            ViewMode::Tool(_) => return None,
        };
        Some(Action::EnterLiveSend(id))
    }

    /// Translate one key event in live-send mode and hand the result to
    /// the background worker. The worker owns the tmux Session and runs
    /// `send-keys` off the UI thread so a slow fork+exec never blocks
    /// the redraw loop; literal-key runs coalesce into a single tmux
    /// call so fast typing isn't N forks. Ctrl+q clears `live_send`
    /// and drops the worker (which closes its channel, exiting the
    /// thread cleanly on the next iteration).
    ///
    /// Before dispatching we re-verify that the target session still
    /// exists at the same tmux name as it had at entry time. If a peer
    /// process deleted the session or a rename diverged the name from
    /// what the worker is targeting, the user would otherwise type
    /// into the void with only a `tracing::warn!` for company. Auto-
    /// exit + info dialog instead.
    fn handle_live_send_key(&mut self, key: KeyEvent) {
        let Some(state) = self.live_send.clone() else {
            return;
        };

        // Leader menu: a prior keystroke matched the configured leader
        // (tmux-style prefix, default Ctrl+B), so this key picks a
        // live-send command instead of being forwarded. Always disarm
        // first so a stray second key can't leave the menu stuck open.
        if self.live_send_pending_leader {
            self.live_send_pending_leader = false;
            // Leader pressed twice: deliver a literal leader keystroke to
            // the agent (matches tmux `send-prefix`), so binding the
            // leader never fully steals the chord from downstream programs.
            if let Some(leader) = state.leader {
                if live_send::chord_matches(leader, key) {
                    if let live_send::LiveDispatch::Send(tmux_key) = live_send::translate(key) {
                        if let Some(worker) = &self.live_send_worker {
                            worker.send(tmux_key);
                        }
                    }
                    return;
                }
            }
            // Command letters match only when unmodified: the leader-again
            // passthrough above already claimed the modified form (`C-b`),
            // and folding `Ctrl+K` / `Alt+b` into a command would surprise
            // users reaching for a modified chord. Shift is allowed since
            // it just yields the uppercase code.
            let plain = !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
            match key.code {
                KeyCode::Char('k') | KeyCode::Char('K') if plain => self.open_command_palette(),
                KeyCode::Char('b') | KeyCode::Char('B') if plain => self.toggle_sidebar_collapsed(),
                KeyCode::Char('q') | KeyCode::Char('Q') if plain => {
                    self.exit_live_send_and_restore_sizing(&state)
                }
                // Esc (or any unbound / modified key) cancels the menu
                // without forwarding: the leader already swallowed this
                // keystroke, and tmux's prefix behaves the same way for
                // unknown keys.
                _ => {}
            }
            return;
        }

        // `handle_key` already cleared any finalized preview
        // selection at the top, so the highlight doesn't linger
        // across the keystroke that switched the user out of
        // copy-and-look mode. The PageUp/PageDown scroll keys below
        // would otherwise need their own dismissal; the shared
        // top-of-handle_key clear covers them too.

        // Shift+PageUp / Shift+PageDown scroll the preview pane
        // without forwarding to the agent. Matches the terminal-
        // emulator convention (xterm, gnome-terminal, iTerm, etc.)
        // where shift+page operates on the outer scrollback, not the
        // inner program. Bare PageUp/PageDown still goes to the agent
        // so agents that page their own UI keep working.
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if shift && !ctrl && !alt {
            const PAGE_STEP: u16 = 10;
            match key.code {
                KeyCode::PageUp => {
                    self.preview_scroll_offset =
                        self.preview_scroll_offset.saturating_add(PAGE_STEP);
                    return;
                }
                KeyCode::PageDown => {
                    self.preview_scroll_offset =
                        self.preview_scroll_offset.saturating_sub(PAGE_STEP);
                    return;
                }
                _ => {}
            }
        }

        // Exit-chord check runs before the drift check: exiting is
        // always safe, and the user pressing the chord to escape a
        // drifted/stuck live mode shouldn't hit a "session ended"
        // dialog on the way out.
        if live_send::chord_list_matches(&state.exit_chords, key) {
            self.exit_live_send_and_restore_sizing(&state);
            return;
        }
        // Leader (prefix) press: arm the live-send command menu and
        // swallow the keystroke. The next key is handled by the
        // pending-leader branch at the top. Checked after the exit chord
        // so a misconfigured leader == exit chord still exits.
        if let Some(leader) = state.leader {
            if live_send::chord_matches(leader, key) {
                self.live_send_pending_leader = true;
                return;
            }
        }
        if let Some(reason) = self.live_send_drift_reason(&state) {
            self.exit_live_send_and_restore_sizing(&state);
            self.info_dialog = Some(InfoDialog::new("Live send ended", reason));
            return;
        }
        match live_send::translate(key) {
            live_send::LiveDispatch::Ignore => {}
            live_send::LiveDispatch::Send(tmux_key) => {
                if let Some(worker) = &self.live_send_worker {
                    worker.send(tmux_key);
                }
                self.stamp_last_accessed(&state.session_id);
            }
        }
    }

    /// Tear down live-send state and restore the tmux window's
    /// automatic sizing policy. live-send's per-keystroke resize loop
    /// forces tmux into manual sizing; if we leave it that way, the
    /// next tmux attach from a full-size terminal stays cramped at
    /// the preview-pane dimensions live-send left behind. Re-setting
    /// `window-size latest` is best-effort: failures are swallowed so
    /// a stuck pane never blocks the user's exit.
    fn exit_live_send_and_restore_sizing(&mut self, state: &live_send::LiveSendState) {
        let session = crate::tmux::Session::from_name(&state.tmux_name);
        session.reset_size_to_latest_client();
        self.live_send = None;
        self.live_send_worker = None;
        // Leave the capture worker running: the same pane is still
        // previewed after exit, just at the idle cadence. The render
        // reconcile retunes it (and retargets if the view later changes).
        self.live_send_last_resize = None;
        // The leader menu and sidebar collapse are live-mode-only: drop
        // any half-entered leader chord and re-reveal the session list so
        // the normal home view is never left in a collapsed or armed state.
        self.live_send_pending_leader = false;
        self.sidebar_collapsed = false;
        // Live mode just owned the pane's size; the non-live preview must
        // re-assert its geometry on the next render now that the header is
        // visible again (and so the agent reflows back to the previewed size).
        self.preview_pane_synced = None;
        // Preview selections also work outside live mode now, but a
        // live-mode highlight pins to the live-resized pane coords,
        // and exiting reflows the preview back to its normal size.
        // Drop the selection so the highlight can't survive into a
        // pane it no longer points at.
        self.clear_preview_selection();
    }

    /// Returns `Some(reason)` if the live-send target has drifted out
    /// from under us between entry and now. Three drift modes:
    /// - Instance row deleted (peer / web structured view / another aoe killed
    ///   it).
    /// - Title renamed (which regenerates the tmux session name; the
    ///   worker is now targeting a stale name).
    /// - tmux session itself is gone (`tmux kill-session`, server
    ///   restart) even though our instance row says otherwise. We use
    ///   the existing `session_exists_from_cache` lookup so this costs
    ///   a hashmap probe per keystroke (the status poller refreshes
    ///   the cache every 500ms anyway). If the cache has no entry
    ///   (`None`, e.g. before first refresh) we don't claim drift; the
    ///   instance + name checks above are still the load-bearing
    ///   safety net.
    ///
    /// The caller uses the message verbatim in the info dialog, so
    /// phrase it as a user-facing sentence.
    fn live_send_drift_reason(&self, state: &live_send::LiveSendState) -> Option<&'static str> {
        let Some(inst) = self.get_instance(&state.session_id) else {
            return Some("Session was deleted while live mode was active.");
        };
        let current_name = match state.target {
            live_send::LiveSendTarget::Agent => {
                crate::tmux::Session::generate_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::Terminal => {
                crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                crate::tmux::ContainerTerminalSession::generate_name(&inst.id, &inst.title)
            }
        };
        if current_name != state.tmux_name {
            return Some("Session was renamed while live mode was active.");
        }
        if crate::tmux::session_exists_from_cache(&state.tmux_name) == Some(false) {
            return Some("tmux pane went away while live mode was active.");
        }
        None
    }

    /// Open the send-message dialog for the currently-selected running session.
    /// If pending_paste has accumulated text from earlier untargeted pastes,
    /// drain it into the dialog so voice/dictation captured before a session
    /// was picked still gets used. No-op if no running session is targetable.
    ///
    /// Honors `view_mode`: in Terminal view, the dialog targets the paired
    /// terminal pane (host or container) rather than the agent, so 'm'
    /// composes a command for the same shell the user is previewing.
    fn open_send_message_dialog(&mut self) {
        let Some((id, title, target)) = self.resolve_send_target() else {
            return;
        };
        let label = live_send::format_target_label(&title, target);
        self.pending_send_session = Some(id);
        self.pending_send_target = target;
        let mut dialog = SendMessageDialog::new(&label);
        if let Some(buf) = self.pending_paste.take() {
            if !buf.is_empty() {
                dialog.handle_paste(&buf);
            }
        }
        self.send_message_dialog = Some(dialog);
    }

    /// Compose target for the current view: agent in Structured view, the
    /// paired host/container terminal in Terminal view. Tool view has
    /// no clean compose target (the tool owns the pane), so it falls
    /// through to Agent for the historical paste/letter-capture path.
    pub(super) fn current_send_target(&self) -> live_send::LiveSendTarget {
        match &self.view_mode {
            ViewMode::Structured => live_send::LiveSendTarget::Agent,
            ViewMode::Terminal => {
                if let Some(id) = self.selected_session.as_deref() {
                    if let Some(inst) = self.get_instance(id) {
                        if inst.is_sandboxed()
                            && self.get_terminal_mode(id) == TerminalMode::Container
                        {
                            return live_send::LiveSendTarget::ContainerTerminal;
                        }
                    }
                }
                live_send::LiveSendTarget::Terminal
            }
            ViewMode::Tool(_) => live_send::LiveSendTarget::Agent,
        }
    }

    /// Resolve `(id, title, target)` for an untargeted paste, 'm', or
    /// strict-mode letter capture. Agent targets keep the historical
    /// gate (the agent tmux pane must already exist) so the compose
    /// dialog can't open against a stopped session; terminal targets
    /// relax that gate because `execute_send_message` will spawn the
    /// paired terminal on demand the same way `attach_terminal` does.
    fn resolve_send_target(&self) -> Option<(String, String, live_send::LiveSendTarget)> {
        let id = self.selected_session.as_ref()?;
        let inst = self.get_instance(id)?;
        if matches!(inst.status, Status::Creating | Status::Deleting) {
            return None;
        }
        let target = self.current_send_target();
        let ready = match target {
            live_send::LiveSendTarget::Agent => crate::tmux::Session::new(&inst.id, &inst.title)
                .map(|s| s.exists())
                .unwrap_or(false),
            live_send::LiveSendTarget::Terminal | live_send::LiveSendTarget::ContainerTerminal => {
                true
            }
        };
        if !ready {
            return None;
        }
        Some((inst.id.clone(), inst.title.clone(), target))
    }

    /// Strict-mode typing guard: a bare lowercase letter was pressed outside
    /// navigation (j/k/h/l). Treat it as inadvertent typing; open the compose
    /// dialog for the selected session pre-filled with that character. Mirrors
    /// handle_paste's dialog-delegation + fallback logic.
    fn capture_letter_to_compose(&mut self, c: char) {
        let s = c.to_string();
        if let Some(ref mut dialog) = self.send_message_dialog {
            dialog.handle_paste(&s);
            return;
        }
        if let Some(ref mut dialog) = self.new_dialog {
            dialog.handle_paste(&s);
            return;
        }
        if let Some(ref mut dialog) = self.rename_dialog {
            dialog.handle_paste(&s);
            return;
        }
        if let Some(ref mut dialog) = self.worktree_name_dialog {
            dialog.handle_paste(&s);
            return;
        }

        if let Some((id, title, target)) = self.resolve_send_target() {
            let label = live_send::format_target_label(&title, target);
            self.pending_send_session = Some(id);
            self.pending_send_target = target;
            let mut dialog = SendMessageDialog::new(&label);
            dialog.handle_paste(&s);
            self.send_message_dialog = Some(dialog);
            return;
        }

        match self.pending_paste.as_mut() {
            Some(buf) => buf.push_str(&s),
            None => self.pending_paste = Some(s),
        }
    }

    /// Re-score matches after a reload without moving the cursor.
    pub(super) fn refresh_search_matches(&mut self) {
        let query = self.search_query.value();
        if query.is_empty() {
            self.search_matches.clear();
            self.search_match_index = 0;
            return;
        }

        use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
        use nucleo_matcher::{Config, Matcher, Utf32Str};

        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let atom = Atom::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );

        let mut scored: Vec<(usize, u16)> = Vec::new();
        let mut buf = Vec::new();

        for (idx, item) in self.flat_items.iter().enumerate() {
            let haystack = match item {
                Item::Session { id, .. } => {
                    if let Some(inst) = self.get_instance(id) {
                        format!("{} {}", inst.title, inst.project_path)
                    } else {
                        continue;
                    }
                }
                Item::Group { name, path, .. } => {
                    format!("{} {}", name, path)
                }
            };

            let haystack_utf32 = Utf32Str::new(&haystack, &mut buf);
            if let Some(score) = atom.score(haystack_utf32, &mut matcher) {
                scored.push((idx, score));
            }
        }

        scored.sort_by_key(|a| std::cmp::Reverse(a.1));
        self.search_matches = scored.into_iter().map(|(idx, _)| idx).collect();
        // Clamp match_index in case matches shrank
        if self.search_matches.is_empty() {
            self.search_match_index = 0;
        } else if self.search_match_index >= self.search_matches.len() {
            self.search_match_index = self.search_matches.len() - 1;
        }
    }

    pub(super) fn update_search(&mut self) {
        self.search_matches.clear();
        self.search_match_index = 0;

        let query = self.search_query.value();
        if query.is_empty() {
            return;
        }

        use nucleo_matcher::pattern::{Atom, AtomKind, CaseMatching, Normalization};
        use nucleo_matcher::{Config, Matcher, Utf32Str};

        let mut matcher = Matcher::new(Config::DEFAULT.match_paths());
        let atom = Atom::new(
            query,
            CaseMatching::Ignore,
            Normalization::Smart,
            AtomKind::Fuzzy,
            false,
        );

        let mut scored: Vec<(usize, u16)> = Vec::new();
        let mut buf = Vec::new();

        for (idx, item) in self.flat_items.iter().enumerate() {
            let haystack = match item {
                Item::Session { id, .. } => {
                    if let Some(inst) = self.get_instance(id) {
                        format!("{} {}", inst.title, inst.project_path)
                    } else {
                        continue;
                    }
                }
                Item::Group { name, path, .. } => {
                    format!("{} {}", name, path)
                }
            };

            let haystack_utf32 = Utf32Str::new(&haystack, &mut buf);
            if let Some(score) = atom.score(haystack_utf32, &mut matcher) {
                scored.push((idx, score));
            }
        }

        scored.sort_by_key(|a| std::cmp::Reverse(a.1));
        self.search_matches = scored.into_iter().map(|(idx, _)| idx).collect();

        if let Some(&best) = self.search_matches.first() {
            self.cursor = best;
            self.update_selected();
        }
    }

    /// Gate sandbox session creation on a one-time confirmation when the resolved
    /// config has glob `volume_ignores` (e.g. `**/bin`). Those entries are expanded
    /// against the workspace at create time, a point-in-time snapshot that won't
    /// shadow directories a build creates later inside the container (#2045). Shows
    /// the dialog once (unless already acknowledged or no glob is configured),
    /// otherwise proceeds straight to creation.
    fn maybe_confirm_volume_ignores_globs(&mut self, data: NewSessionData) -> Option<Action> {
        if data.sandbox && !Self::volume_ignores_globs_acknowledged() {
            if let Some(message) = Self::volume_ignores_glob_confirm_message(&data) {
                self.volume_ignores_glob_dialog = Some(
                    crate::tui::dialogs::ConfirmDialog::new(
                        "Glob volume_ignores",
                        &message,
                        "volume_ignores_globs",
                    )
                    .neutral()
                    .offering_dont_ask_again(),
                );
                self.pending_volume_ignores_glob_data = Some(data);
                return None;
            }
        }
        self.continue_session_creation(data)
    }

    fn volume_ignores_globs_acknowledged() -> bool {
        load_config()
            .ok()
            .flatten()
            .map(|c| c.app_state.has_acknowledged_volume_ignores_globs)
            .unwrap_or(false)
    }

    fn persist_volume_ignores_globs_ack(&self) {
        if let Ok(mut config) = load_config().map(|c| c.unwrap_or_default()) {
            config.app_state.has_acknowledged_volume_ignores_globs = true;
            if let Err(e) = save_config(&config) {
                tracing::warn!(target: "tui.input", "Failed to save volume_ignores ack: {e}");
            }
        }
    }

    /// Build the confirm message describing how this session's glob volume_ignores
    /// will expand, or `None` when there is no glob entry (nothing to confirm).
    fn volume_ignores_glob_confirm_message(data: &NewSessionData) -> Option<String> {
        let config =
            repo_config::resolve_config_with_repo(&data.profile, std::path::Path::new(&data.path))
                .ok()?;
        let expansions = crate::session::container_config::preview_glob_volume_ignores(
            &data.path,
            None,
            &config.sandbox.volume_ignores,
        )
        .ok()?;
        if expansions.is_empty() {
            return None;
        }
        let match_count: usize = expansions
            .iter()
            .map(|e| e.matched_container_paths.len())
            .sum();
        // Name the patterns (capped) so the user sees what will expand.
        let mut patterns: Vec<&str> = expansions.iter().map(|e| e.pattern.as_str()).collect();
        let pattern_list = if patterns.len() > 3 {
            patterns.truncate(3);
            format!("{}, ...", patterns.join(", "))
        } else {
            patterns.join(", ")
        };
        Some(format!(
            "volume_ignores globs ({}) match {} director{} in the workspace right now. Each \
             becomes an ignore mount at create time; directories a build creates later inside the \
             container are not hidden. Proceed?",
            pattern_list,
            match_count,
            if match_count == 1 { "y" } else { "ies" },
        ))
    }

    /// Continue session creation after agent hooks acknowledgment.
    /// Runs the repo hook trust check and then creates the session.
    fn continue_session_creation(&mut self, data: NewSessionData) -> Option<Action> {
        use crate::session::TrustSurface;
        let trust = match repo_config::check_repo_trust(std::path::Path::new(&data.path)) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(target: "tui.input", "Failed to check repo trust: {}", e);
                let fallback = repo_config::resolve_global_profile_hooks(&data.profile);
                return self.create_session_with_hooks(data, fallback);
            }
        };

        let repo_hooks: Option<crate::session::HooksConfig> = match &trust.hooks {
            TrustSurface::Trusted(h) | TrustSurface::NeedsTrust { config: h, .. } => {
                Some(h.clone())
            }
            TrustSurface::Absent => None,
        };
        let hooks_hash = match &trust.hooks {
            TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
            _ => None,
        };
        let mcp_hash = match &trust.mcp {
            TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
            _ => None,
        };
        let mcp_servers = match &trust.mcp {
            TrustSurface::Trusted(s) | TrustSurface::NeedsTrust { config: s, .. } => s.clone(),
            TrustSurface::Absent => Vec::new(),
        };

        // Hooks to run if approved (repo hooks, else global) vs skipped
        // (already-trusted repo hooks still run; newly-prompted hooks fall back
        // to the global set, matching the prior TUI skip behavior).
        let hooks_on_trust = match &repo_hooks {
            Some(h) => repo_config::merge_hooks_with_config(&data.profile, h.clone()),
            None => repo_config::resolve_global_profile_hooks(&data.profile),
        };
        let hooks_on_skip = match &trust.hooks {
            TrustSurface::Trusted(h) => {
                repo_config::merge_hooks_with_config(&data.profile, h.clone())
            }
            _ => repo_config::resolve_global_profile_hooks(&data.profile),
        };

        if !trust.needs_prompt() {
            return self.create_session_with_hooks(data, hooks_on_trust);
        }

        use crate::tui::dialogs::RepoTrustDialog;
        let merged_hooks = repo_hooks
            .as_ref()
            .map(|h| repo_config::merge_hooks_for_display(&data.profile, h))
            .unwrap_or_default();
        self.repo_trust_dialog = Some(RepoTrustDialog::new(
            merged_hooks,
            repo_hooks.unwrap_or_default(),
            mcp_servers,
            hooks_on_trust,
            hooks_on_skip,
            hooks_hash,
            mcp_hash,
            data.path.clone(),
        ));
        self.pending_repo_trust_data = Some(data);
        None
    }

    /// Create a session with optional hooks. Delegates to the background
    /// `CreationPoller` when hooks are present, when the session is sandboxed,
    /// or when a worktree branch is requested (to avoid freezing the TUI on
    /// slow git hooks like `post-checkout`).
    pub(super) fn create_session_with_hooks(
        &mut self,
        data: NewSessionData,
        hooks: Option<crate::session::HooksConfig>,
    ) -> Option<Action> {
        let has_hooks = hooks
            .as_ref()
            .is_some_and(|h| !h.on_create.is_empty() || !h.on_launch.is_empty());
        let has_worktree = data.worktree_enabled;

        if data.sandbox || has_hooks || has_worktree {
            self.request_creation(data, hooks);
            return None;
        }

        match self.create_session(data) {
            Ok(session_id) => {
                self.new_dialog = None;
                Some(Action::AttachAfterCreate(session_id))
            }
            Err(e) => {
                tracing::error!(target: "tui.input", "Failed to create session: {}", e);
                if let Some(dialog) = &mut self.new_dialog {
                    dialog.set_error(e.to_string());
                }
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::{SessionConfig, ToolSessionConfig};

    #[test]
    fn format_target_label_distinguishes_terminal_panes() {
        // Users firing 'm' from Terminal view should see the dialog
        // title (and the live-mode banner) call out the target pane so
        // they don't accidentally send agent prompts into a shell (or
        // vice versa). Both the compose dialog and the status-bar
        // banner route through the same helper so the label can't drift.
        use live_send::{format_target_label, LiveSendTarget};
        assert_eq!(
            format_target_label("my-session", LiveSendTarget::Agent),
            "my-session",
        );
        assert_eq!(
            format_target_label("my-session", LiveSendTarget::Terminal),
            "my-session (terminal)",
        );
        assert_eq!(
            format_target_label("my-session", LiveSendTarget::ContainerTerminal),
            "my-session (container)",
        );
    }

    #[test]
    fn hook_install_agent_uses_detect_as_for_custom_codex_wrapper() {
        let mut config = SessionConfig::default();
        config
            .agent_detect_as
            .insert("wrapped-codex".to_string(), "codex".to_string());

        let agent = resolve_hook_install_agent("wrapped-codex", &config).unwrap();

        assert_eq!(agent.name, "codex");
    }

    #[test]
    fn hook_install_agent_keeps_builtin_agent_resolution_first() {
        let mut config = SessionConfig::default();
        config
            .agent_detect_as
            .insert("opencode".to_string(), "codex".to_string());

        assert!(resolve_hook_install_agent("opencode", &config).is_none());
    }

    #[test]
    fn hook_install_agent_ignores_unknown_detect_as_target() {
        let mut config = SessionConfig::default();
        config
            .agent_detect_as
            .insert("wrapped-agent".to_string(), "missing-agent".to_string());

        assert!(resolve_hook_install_agent("wrapped-agent", &config).is_none());
    }

    #[test]
    fn parse_hotkey_accepts_alt_letter() {
        let (code, mods) = parse_hotkey("Alt+g").expect("valid");
        assert_eq!(code, KeyCode::Char('g'));
        assert_eq!(mods, KeyModifiers::ALT);
    }

    #[test]
    fn parse_hotkey_is_case_insensitive_on_modifier() {
        assert!(parse_hotkey("alt+g").is_some());
        assert!(parse_hotkey("ALT+g").is_some());
        assert!(parse_hotkey("aLt+g").is_some());
    }

    #[test]
    fn parse_hotkey_normalizes_letter_to_lowercase() {
        let (code, _) = parse_hotkey("Alt+G").expect("valid");
        assert_eq!(code, KeyCode::Char('g'));
    }

    #[test]
    fn parse_hotkey_accepts_digit() {
        let (code, mods) = parse_hotkey("Alt+1").expect("valid");
        assert_eq!(code, KeyCode::Char('1'));
        assert_eq!(mods, KeyModifiers::ALT);
    }

    #[test]
    fn parse_hotkey_rejects_non_alt_modifier() {
        assert!(parse_hotkey("Ctrl+g").is_none());
        assert!(parse_hotkey("Shift+g").is_none());
        assert!(parse_hotkey("Cmd+g").is_none());
    }

    #[test]
    fn parse_hotkey_rejects_multi_char_key() {
        assert!(parse_hotkey("Alt+gg").is_none());
        assert!(parse_hotkey("Alt+F1").is_none());
    }

    #[test]
    fn parse_hotkey_rejects_missing_modifier() {
        assert!(parse_hotkey("g").is_none());
        assert!(parse_hotkey("Alt").is_none());
        assert!(parse_hotkey("").is_none());
    }

    #[test]
    fn parse_hotkey_rejects_wrong_separator() {
        assert!(parse_hotkey("Alt-g").is_none());
        assert!(parse_hotkey("Alt g").is_none());
    }

    #[test]
    fn validate_tool_hotkeys_reports_each_invalid_entry() {
        let mut tools = std::collections::HashMap::new();
        tools.insert(
            "lazygit".to_string(),
            ToolSessionConfig {
                command: "lazygit".into(),
                hotkey: Some("Alt+g".into()),
            },
        );
        tools.insert(
            "yazi".to_string(),
            ToolSessionConfig {
                command: "yazi".into(),
                hotkey: Some("Ctrl+f".into()),
            },
        );
        tools.insert(
            "tig".to_string(),
            ToolSessionConfig {
                command: "tig".into(),
                hotkey: Some("Alt+too-long".into()),
            },
        );
        let warnings = validate_tool_hotkeys(&tools);
        assert_eq!(warnings.len(), 2);
        let joined = warnings.join("|");
        assert!(joined.contains("yazi"));
        assert!(joined.contains("tig"));
        assert!(!joined.contains("lazygit"));
    }

    #[test]
    fn validate_tool_hotkeys_empty_when_all_valid_or_unset() {
        let mut tools = std::collections::HashMap::new();
        tools.insert(
            "lazygit".to_string(),
            ToolSessionConfig {
                command: "lazygit".into(),
                hotkey: Some("Alt+g".into()),
            },
        );
        tools.insert(
            "rg".to_string(),
            ToolSessionConfig {
                command: "rg --files".into(),
                hotkey: None,
            },
        );
        assert!(validate_tool_hotkeys(&tools).is_empty());
    }

    #[test]
    fn build_tool_hotkey_cache_sorts_by_name_and_skips_invalid() {
        let mut tools = std::collections::HashMap::new();
        tools.insert(
            "zoxide".to_string(),
            ToolSessionConfig {
                command: "z".into(),
                hotkey: Some("Alt+z".into()),
            },
        );
        tools.insert(
            "lazygit".to_string(),
            ToolSessionConfig {
                command: "lazygit".into(),
                hotkey: Some("Alt+g".into()),
            },
        );
        tools.insert(
            "broken".to_string(),
            ToolSessionConfig {
                command: "x".into(),
                hotkey: Some("Ctrl+x".into()),
            },
        );
        tools.insert(
            "no-hotkey".to_string(),
            ToolSessionConfig {
                command: "y".into(),
                hotkey: None,
            },
        );

        let cache = build_tool_hotkey_cache(&tools);
        // Two valid entries, sorted by name.
        assert_eq!(cache.len(), 2);
        assert_eq!(cache[0].0, "lazygit");
        assert_eq!(cache[0].1, KeyCode::Char('g'));
        assert_eq!(cache[0].2, KeyModifiers::ALT);
        assert_eq!(cache[1].0, "zoxide");
        assert_eq!(cache[1].1, KeyCode::Char('z'));
    }

    #[test]
    fn build_tool_hotkey_cache_tie_break_favors_alphabetically_first_name() {
        let mut tools = std::collections::HashMap::new();
        // Both bind Alt+g; alphabetical winner is "alpha".
        tools.insert(
            "beta".to_string(),
            ToolSessionConfig {
                command: "b".into(),
                hotkey: Some("Alt+g".into()),
            },
        );
        tools.insert(
            "alpha".to_string(),
            ToolSessionConfig {
                command: "a".into(),
                hotkey: Some("Alt+g".into()),
            },
        );
        let cache = build_tool_hotkey_cache(&tools);
        assert_eq!(cache[0].0, "alpha");
        assert_eq!(cache[1].0, "beta");
    }

    fn glob_session_data(path: &str) -> NewSessionData {
        NewSessionData {
            profile: String::new(),
            title: String::new(),
            path: path.to_string(),
            group: String::new(),
            tool: "claude".to_string(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            extra_repo_paths: Vec::new(),
            sandbox: true,
            sandbox_image: "ubuntu:latest".to_string(),
            yolo_mode: false,
            extra_env: Vec::new(),
            extra_args: String::new(),
            command_override: String::new(),
            scratch: false,
        }
    }

    /// The confirm gate only fires when the resolved config has a glob ignore
    /// that the message can name; literal-only ignores produce no dialog.
    #[test]
    #[serial_test::serial]
    fn volume_ignores_glob_confirm_message_fires_only_on_globs() {
        let temp_home = tempfile::TempDir::new().unwrap();
        std::env::set_var("HOME", temp_home.path());
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        std::env::set_var("XDG_CONFIG_HOME", temp_home.path().join(".config"));

        let project = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(project.path().join("src/App/bin")).unwrap();
        git2::Repository::init(project.path()).unwrap();
        let cfg = project.path().join(".agent-of-empires");
        std::fs::create_dir_all(&cfg).unwrap();
        let path = project.path().to_str().unwrap();

        std::fs::write(
            cfg.join("config.toml"),
            "[sandbox]\nvolume_ignores = [\"**/bin\", \"target\"]\n",
        )
        .unwrap();
        let msg = HomeView::volume_ignores_glob_confirm_message(&glob_session_data(path))
            .expect("glob ignore should produce a confirm message");
        assert!(msg.contains("**/bin"), "message names the pattern: {msg}");
        assert!(msg.contains("1 directory"), "one match counted: {msg}");

        std::fs::write(
            cfg.join("config.toml"),
            "[sandbox]\nvolume_ignores = [\"target\", \".venv\"]\n",
        )
        .unwrap();
        assert!(
            HomeView::volume_ignores_glob_confirm_message(&glob_session_data(path)).is_none(),
            "literal-only ignores must not trigger the gate"
        );
    }
}
