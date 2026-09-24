//! Input handling for HomeView

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::Position;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::bindings::{self, ActionId};
use super::{
    live_send, DragKind, HomeView, PermissionResponseTarget, PreviewSelection, TerminalMode,
    ViewMode,
};
use crate::session::config::repo_config;
use crate::session::config::{
    load_config, update_app_state, update_config, GroupByMode, SidebarPosition, SortOrder,
};
use crate::session::{list_profiles_for_display, Item, Status};
use crate::tui::app::Action;
use crate::tui::dialogs::ServeAction;
use crate::tui::dialogs::{
    builtin_commands, CommandPaletteDialog, ConfirmDialog, ContextMenuAction, ContextMenuDialog,
    DeleteDialogConfig, DialogResult, GroupDeleteOptionsDialog, HooksInstallDialog, InfoDialog,
    IntroOutcome, NewSessionData, NewSessionDialog, NoAgentsAction, PaletteAction, PaletteCommand,
    PaletteGroup, ProfilePickerAction, ProjectsDialog, RenameDialog, RenameMode, RepoTrustAction,
    RestartDialog, SendMessageDialog, TipsDialog, TipsOutcome, UnifiedDeleteDialog,
    WorktreeNameDialog,
};
use crate::tui::diff::{DiffAction, DiffView};
use crate::tui::responsive;
use crate::tui::settings::{SettingsAction, SettingsView};

/// Longest gap between two left-clicks on one row that still counts as a double-click;
/// 400ms matches most desktop environments.
const DOUBLE_CLICK_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(400);

/// The two synthetic bottom-of-sidebar sections. Their headers look like groups in
/// `flat_items` but carry sentinel paths, so the section-scoped context-menu actions
/// resolve which one the cursor is on here instead of repeating the path checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SidebarSection {
    Trash,
    Archived,
}

/// Persist the first-run intro picks: theme name to `config.theme.name`, attach mode to
/// `default_attach_mode`. Failures are logged and swallowed so a config write hiccup
/// cannot block startup.
fn apply_intro_outcome(outcome: &IntroOutcome) {
    if outcome.final_theme.is_none()
        && outcome.final_attach_mode.is_none()
        && outcome.telemetry_opt_in.is_none()
    {
        return;
    }
    let result = update_config(|config| {
        if let Some(theme) = &outcome.final_theme {
            config.theme.name = theme.clone();
        }
        if let Some(mode) = outcome.final_attach_mode {
            config.session.default_attach_mode = mode;
        }
        if let Some(opt_in) = outcome.telemetry_opt_in {
            config.telemetry.enabled = opt_in;
        }
    });
    if let Err(e) = result {
        tracing::warn!(target: "tui.input", "Failed to persist intro outcome: {e}");
    }
    if outcome.telemetry_opt_in.is_some() {
        if let Err(e) = update_app_state(|state| {
            state.has_responded_to_telemetry = true;
        }) {
            tracing::warn!(target: "tui.input", "Failed to persist intro outcome: {e}");
        }
    }
    // Sync the install id with the saved opt-in choice (no-op under
    // DO_NOT_TRACK). Done after save so telemetry.json matches config.
    if let Some(opt_in) = outcome.telemetry_opt_in {
        crate::telemetry::apply_opt_in_change(opt_in);
    }
}

/// Persist the answer to the telemetry consent popup: set the opt-in flag, mark the
/// prompt answered so it never returns, and reconcile the install id.
fn persist_telemetry_consent(opt_in: bool) {
    if let Err(e) = update_config(|config| {
        config.telemetry.enabled = opt_in;
    }) {
        tracing::warn!(target: "tui.input", "Failed to persist telemetry consent: {e}");
    }
    if let Err(e) = update_app_state(|state| {
        state.has_responded_to_telemetry = true;
    }) {
        tracing::warn!(target: "tui.input", "Failed to persist telemetry consent: {e}");
    }
    crate::telemetry::apply_opt_in_change(opt_in);
}

/// Decompose pasted text into `TmuxKey`s safe for the live-send worker.
///
/// A single-line paste travels as one `Literal`, skipping the bracketed-paste wrapping,
/// so a bare shell or an agent that never enabled `\e[?2004h` does not render the markers
/// as text; tabs still go through as `Named("Tab")`.
///
/// A multi-line paste goes to tmux as one `Paste` action, delivered with `load-buffer` +
/// `paste-buffer -p`, so the agent sees one paste rather than N Enter presses (#1546) and
/// tmux brackets it only for panes that set DECSET 2004. `\r\n` pairs coalesce so
/// Windows line endings don't double up, and other control bytes are dropped rather than
/// risk an embedded escape closing the bracketed-paste sequence.
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
    // Hand the payload to tmux as one paste and let `paste-buffer -p` decide about the
    // markers: emitting them ourselves delivered them to every pane, including raw shells
    // and REPLs that never set DECSET 2004, which parse `\e[2` as a partial Insert
    // sequence and self-insert the leftover `00~` / `01~`.
    //
    // tmux replaces LF with CR in the buffer by default, so interior newlines land as
    // before. `\r\n` pairs coalesce here so Windows line endings don't double up, and
    // other control bytes are dropped rather than risk an escape closing the
    // bracketed-paste sequence.
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\n' => out.push('\n'),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\t' => out.push('\t'),
            c if (c as u32) < 0x20 || c == '\x7f' => {}
            c => out.push(c),
        }
    }

    vec![live_send::TmuxKey::Paste(out)]
}

/// The rectangle mouse coordinates map into, or `None` when the pointer is not over the
/// pane that receives input.
///
/// Normally the previewed pane is sized to the preview output rect, so the rect is the
/// pane. On a composited preview the rect is the whole window while input still goes to
/// pane 0 alone (#435, #488), so pane 0's sub-rectangle is the target: mapping against
/// the full rect would report a column past its right edge as though the pane were
/// window-wide. A pointer outside pane 0 is dropped rather than clamped, which would
/// synthesise a click on its border.
///
/// Pane 0 may have a non-zero origin in the composite. The TUI bottom-follows a composite
/// taller than its output, clipping reserved rows above pane 0, so its first visible cell
/// still starts at `pane.x`/`pane.y`; adding `rect.top` here would shift input below the
/// displayed pane.
fn mouse_target_rect(
    cursor: &crate::tmux::PaneCursor,
    pane: ratatui::layout::Rect,
    col: u16,
    row: u16,
) -> Option<ratatui::layout::Rect> {
    // Unsplit: the rect is the pane, and containment stays the caller's business
    // (`hit_preview` gates the press) with `map_pane_cell` clamping, so this must not
    // start rejecting cells that used to clamp.
    if cursor.composite_pane0.is_none() {
        return Some(pane);
    }
    let pane0 = mouse_pane_rect(cursor, pane);
    let inside = col >= pane0.x
        && col < pane0.x.saturating_add(pane0.width)
        && row >= pane0.y
        && row < pane0.y.saturating_add(pane0.height);
    inside.then_some(pane0)
}

/// The input pane's rectangle within the preview, with no containment test: pane 0's
/// sub-rectangle on a composited preview, else the whole preview rect. Split from
/// [`mouse_target_rect`] for mid-gesture events, which are not position-gated so a drag
/// that began on pane 0 completes even after the pointer wanders off it.
fn mouse_pane_rect(
    cursor: &crate::tmux::PaneCursor,
    pane: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    match cursor.composite_pane0 {
        Some(rect) => ratatui::layout::Rect {
            x: pane.x,
            y: pane.y,
            width: rect.width.min(pane.width),
            height: rect.height.min(pane.height),
        },
        None => pane,
    }
}

/// Map a hovered screen cell into a forwarded app's 1-based mouse coordinate space,
/// relative to `pane` and clamped inside it; an unpopulated rect falls back to the
/// top-left cell. Shared by the wheel- and click-forward byte builders.
fn map_pane_cell(pane: ratatui::layout::Rect, col: u16, row: u16) -> (u16, u16) {
    if pane.width == 0 || pane.height == 0 {
        (1u16, 1u16)
    } else {
        let cx = col.saturating_sub(pane.x).min(pane.width - 1) + 1;
        let cy = row.saturating_sub(pane.y).min(pane.height - 1) + 1;
        (cx, cy)
    }
}

/// Build the mouse-wheel bytes to forward to a full-screen app under the live preview.
/// `up` selects wheel-up (button 64) over wheel-down (65); `sgr` selects the SGR (1006)
/// encoding over legacy X10, matching whatever the app enabled.
fn wheel_mouse_bytes(
    up: bool,
    sgr: bool,
    pane: ratatui::layout::Rect,
    col: u16,
    row: u16,
) -> Vec<u8> {
    let (cx, cy) = map_pane_cell(pane, col, row);
    let button: u16 = if up { 64 } else { 65 };
    if sgr {
        // SGR (1006): textual, press marker `M`. No coordinate limit.
        format!("\x1b[<{button};{cx};{cy}M").into_bytes()
    } else {
        // Legacy X10: `ESC [ M` then three bytes, each value + 32. Bytes top out at 255,
        // so clamp coordinates at 223; preview cells are far below that.
        let enc = |v: u16| (v.min(223) + 32) as u8;
        vec![0x1b, b'[', b'M', enc(button), enc(cx), enc(cy)]
    }
}

/// Build the bytes for one forwarded mouse button event at screen cell `(col, row)`,
/// mapped into the app's pane. `base_button` is the SGR low-bits code (left=0, middle=1,
/// right=2), `release` a button-up, `motion` a drag. Mirrors `wheel_mouse_bytes`, which
/// covers the wheel buttons.
fn mouse_event_bytes(
    base_button: u16,
    release: bool,
    motion: bool,
    sgr: bool,
    pane: ratatui::layout::Rect,
    col: u16,
    row: u16,
) -> Vec<u8> {
    let (cx, cy) = map_pane_cell(pane, col, row);
    // The motion bit (32) rides on press/drag reports in both encodings.
    let cb = base_button + if motion { 32 } else { 0 };
    if sgr {
        // SGR (1006): press/drag end with `M`, release with `m`; the button
        // identity survives on release (unlike X10).
        let end = if release { 'm' } else { 'M' };
        format!("\x1b[<{cb};{cx};{cy}{end}").into_bytes()
    } else {
        // Legacy X10: `ESC [ M` then three bytes, each value + 32 (clamped at
        // 223). A release can't carry a button, so it uses the agnostic 3.
        let enc = |v: u16| (v.min(223) + 32) as u8;
        let btn = if release { 3 } else { cb };
        vec![0x1b, b'[', b'M', enc(btn), enc(cx), enc(cy)]
    }
}

/// The bare mouse-motion (hover) bytes to forward to the previewed pane, or `None` when
/// the app didn't ask for them: only an app in any-event tracking (DEC 1003) gets bare
/// motion, since a 1000/1002 app never expects a no-button report. The report is the
/// button-agnostic code 3 plus the motion bit, what a real terminal emits, so an app that
/// highlights content under the pointer reacts as it would over a direct attach.
fn hover_forward_bytes(
    cursor: &crate::tmux::PaneCursor,
    pane: ratatui::layout::Rect,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    let target = mouse_target_rect(cursor, pane, col, row)?;
    (cursor.alternate_on && cursor.mouse_all)
        .then(|| mouse_event_bytes(3, false, true, cursor.mouse_sgr, target, col, row))
}

/// Page presses per wheel notch for a no-mouse full-screen app: such apps scroll on
/// `PageUp`/`PageDown` with no finer keyboard step, and a page per notch reads as a
/// normal flick to scroll.
const WHEEL_PAGE_STEP: usize = 1;

/// What to forward to the previewed full-screen pane for one wheel notch, or `None` to
/// fall back to the capture-window scroll. Pure, so the branch (page keys vs raw mouse
/// bytes vs no forward) is asserted without a worker; see `forward_wheel_to_preview`.
fn wheel_forward_key(
    cursor: &crate::tmux::PaneCursor,
    up: bool,
    pane: ratatui::layout::Rect,
    col: u16,
    row: u16,
) -> Option<live_send::TmuxKey> {
    if !cursor.alternate_on {
        return None;
    }
    // Outside pane 0 on a composited preview there is nothing to drive, and paging pane 0
    // because the wheel turned elsewhere would be a scroll the user did not aim.
    let target = mouse_target_rect(cursor, pane, col, row)?;
    if cursor.mouse_tracking {
        Some(live_send::TmuxKey::HexBytes(wheel_mouse_bytes(
            up,
            cursor.mouse_sgr,
            target,
            col,
            row,
        )))
    } else {
        // No mouse tracking: send `PageUp`/`PageDown`, not arrows, which a full-screen
        // app reads as cursor or history navigation. The page keys scroll its transcript
        // regardless of cursor-key mode.
        Some(live_send::TmuxKey::NamedRepeat {
            name: if up { "PageUp" } else { "PageDown" }.to_string(),
            count: WHEEL_PAGE_STEP,
        })
    }
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
        .filter(|agent| agent.hook_config.is_some() || agent.sidecar_hooks.is_some())
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

/// Validate tool hotkey strings, returning human-readable warnings for any that fail to
/// parse. Tool hotkeys must match `Alt+<single-char>`, so a typo surfaces as an error
/// rather than a silently dead binding.
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

/// Sorted lookup of `(tool_name, KeyCode, KeyModifiers)` for every tool whose `hotkey`
/// parses, sorted by name so the alphabetically first tool wins a duplicate. Built at
/// startup and on settings reload, then iterated on every keystroke.
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

/// Pull the cell symbols in columns `[from, to_excl)` out of a parsed scrollback `Line`.
/// The line is laid back out into a one-row buffer at the pane width, so wide characters,
/// combining marks and right-edge truncation resolve exactly as ratatui rendered them.
/// Unwritten cells read as a space, which the caller trims.
fn slice_line_columns(line: &ratatui::text::Line, from: u16, to_excl: u16, width: u16) -> String {
    crate::tui::components::text::line_columns(line, width).slice(from, to_excl.min(width))
}

impl HomeView {
    pub fn is_diff_open(&self) -> bool {
        self.diff_view.is_some()
    }

    /// Whether the full-screen Settings takeover is showing. The wheel scroll gate in
    /// `app.rs` uses this so the whole screen counts as a scroll target, since the
    /// list/preview hit rects it replaced are stale.
    pub fn is_settings_open(&self) -> bool {
        self.settings_view.is_some()
    }

    pub fn hit_preview(&self, col: u16, row: u16) -> bool {
        self.preview_area.contains(Position::from((col, row)))
    }

    /// Drain a theme name queued by intro-dialog clicks. `App` calls this after
    /// `handle_dialog_click` and dispatches `Action::SetTheme`, so the live preview
    /// applies through the same path as the keyboard route.
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

    fn activate_tool(&mut self, tool_name: String, toggle_current: bool) -> Option<Action> {
        let (background, command_is_empty) = self
            .tool_configs
            .get(&tool_name)
            .map(|config| (config.background, config.command.trim().is_empty()))
            .unwrap_or((false, false));

        if !background {
            if toggle_current
                && matches!(&self.view_mode, ViewMode::Tool(current) if current == &tool_name)
            {
                self.view_mode = ViewMode::Structured;
                return None;
            } else {
                self.view_mode = ViewMode::Tool(tool_name);
                self.preview_scroll_offset = 0;
                self.tool_preview_cache = super::PreviewCache::default();
            }
            return self.maybe_auto_start_live_send();
        }

        if command_is_empty {
            self.info_dialog = Some(InfoDialog::new(
                "Tool command missing",
                &format!("Tool '{}' has no command configured", tool_name),
            ));
            return None;
        }

        let Some(session_id) = self.selected_session.clone() else {
            self.info_dialog = Some(InfoDialog::new(
                "No session selected",
                "Select a session before running a background tool.",
            ));
            return None;
        };

        if let Some(inst) = self.get_instance(&session_id) {
            if matches!(inst.status, Status::Creating | Status::Deleting) {
                self.info_dialog = Some(InfoDialog::new(
                    "Session not ready",
                    "This session is still being created or deleted.",
                ));
                return None;
            }
        }

        Some(Action::RunBackgroundToolSession(session_id, tool_name))
    }

    /// Whether the key matches a configured tool session hotkey. On duplicates the
    /// alphabetically first tool name wins, since the cache is sorted by name.
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

    /// The `KeyEvent` a footer-toolbar button at `(col, row)` synthesizes, or `None` when
    /// the click misses every button; the caller routes it through the normal key handler
    /// so a click behaves like the shortcut. `None` while a non-live overlay is open: the
    /// footer is drawn underneath it, but the overlay owns clicks.
    pub fn footer_button_at(&self, col: u16, row: u16) -> Option<KeyEvent> {
        if self.has_non_live_send_overlay() {
            return None;
        }
        let pos = Position::from((col, row));
        self.footer_buttons
            .iter()
            .find(|(rect, _)| rect.contains(pos))
            .map(|(_, key)| *key)
    }

    /// Handle a left-click on the sidebar collapse/expand affordances: the button on the
    /// expanded list's top-right border, or anywhere in the collapsed strip. True when the
    /// click toggled one, so the caller stops before the row-click path. The button sits
    /// inside `list_area`, so this MUST run before `hit_list` or the click falls through
    /// to `handle_empty_list_click` and opens a new session. No-op while a non-live
    /// overlay is open, so the sidebar can't toggle behind a modal.
    pub fn handle_sidebar_collapse_click(&mut self, col: u16, row: u16) -> bool {
        if self.has_non_live_send_overlay() {
            return false;
        }
        let pos = Position::from((col, row));
        if self.collapse_button_area.contains(pos) || self.expand_strip_area.contains(pos) {
            self.toggle_sidebar_collapsed();
            true
        } else {
            false
        }
    }

    /// Open the read-only System Health view when the compact strip is clicked.
    pub fn handle_diagnostics_click(&mut self, col: u16, row: u16) -> bool {
        if self.has_non_live_send_overlay() {
            return false;
        }
        if self.diagnostics_area.contains(Position::from((col, row))) {
            self.open_system_health();
            true
        } else {
            false
        }
    }

    /// Click on the footer tips badge: open the tips overlay, returning true when the
    /// click was on it. Gated on no overlay being open, since the badge rect is captured
    /// behind a modal and a corner click must not punch through.
    pub fn handle_tips_badge_click(&mut self, col: u16, row: u16) -> bool {
        if self.has_non_live_send_overlay() || self.diff_view.is_some() {
            return false;
        }
        let hit = self
            .tips_badge_rect
            .is_some_and(|r| r.contains(Position::from((col, row))));
        if hit {
            self.open_tips_dialog();
        }
        hit
    }

    /// Cancel gestures tied to old screen coordinates before moving the sidebar.
    pub(super) fn set_sidebar_position(&mut self, position: SidebarPosition) {
        if self.sidebar_position == position {
            return;
        }
        match self.drag_state {
            Some(DragKind::ListDivider) => {
                self.handle_drag_end();
            }
            Some(DragKind::PreviewSelect) => {
                self.clear_preview_selection();
            }
            _ => {}
        }
        self.sidebar_position = position;
    }

    /// Hit the preview border shared with the list, on either side.
    /// Stacked and takeover views clear `divider_col`; modals block drags.
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

    /// Begin a drag if `(col, row)` is on the divider or inside the preview pane, in or
    /// out of live mode. True when a drag started, so the caller marks the event handled
    /// and skips the row-click path.
    ///
    /// Divider drags resize the list/preview split. Preview-pane drags start an in-app
    /// text selection: terminal-native drag-select cannot reach the preview because mouse
    /// events are captured for wheel scroll, and one mechanism has to work on Mosh and
    /// mobile clients where Shift-bypass does nothing.
    pub fn handle_drag_start(&mut self, col: u16, row: u16) -> bool {
        if self.hit_divider(col, row) {
            self.drag_state = Some(DragKind::ListDivider);
            return true;
        }
        // Modals that aren't live-send sit over the preview, so a click inside
        // `preview_area` while one is open belongs to the modal and must not seed a hidden
        // selection. `handle_drag_move`'s cancel branch covers a modal opening mid-drag.
        if self.has_non_live_send_overlay() {
            return false;
        }
        // Seed the selection in content coords so it survives a scroll and can span more
        // than one page. `contains` also requires a painted content row, so a drag over an
        // empty pane is a no-op rather than a phantom selection.
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

    /// Resize the list within the preview's minimum width, or update a
    /// text selection within the preview's content bounds.
    ///
    /// True when state changed, so the caller redraws. Nothing persists per tick: the
    /// divider saves on release and the preview-select path emits OSC 52 on release.
    pub fn handle_drag_move(&mut self, col: u16, row: u16) -> bool {
        // Settings scrollbar grab: map the live row to a scroll offset. Handled before the
        // cancel-on-overlay logic below, which keys off `has_dialog()` (true while settings
        // is open) and would tear the drag down on the first move.
        if matches!(self.drag_state, Some(DragKind::SettingsScrollbar)) {
            if let Some(view) = &mut self.settings_view {
                return view.scrollbar_drag_to_row(row);
            }
            // Settings closed mid-drag (shouldn't happen while the button
            // is held); drop the stale drag so the next Up is a no-op.
            self.drag_state = None;
            return false;
        }
        // A dialog opened mid-drag must not keep updating the sidebar invisibly under the
        // modal, so end the drag here and persist whatever width was reached, mirroring
        // `handle_drag_end`.
        //
        // `has_dialog()` is true during live-send, which is exactly when preview
        // drag-select is meant to work, so live mode is exempt; a real modal opening
        // mid-select still kills the drag and drops the selection so it cannot finalize
        // behind the overlay.
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
            Some(DragKind::ListDivider) => {
                if self.divider_col.is_none() {
                    self.handle_drag_end();
                    return false;
                }
                let proposed = match self.sidebar_position {
                    SidebarPosition::Left => col as i32 - self.list_area.x as i32,
                    SidebarPosition::Right => self.list_area.right() as i32 - 1 - col as i32,
                };

                // Match the keyboard shrink limit and reserve preview space.
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
                // Record the live pointer cell so `tick_preview_autoscroll` can keep
                // extending while the cursor is held at the edge. The scroll is not done
                // here: crossterm emits Drag events only on movement, so scrolling
                // per-event makes a held cursor stall and a moving one lurch.
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
            // Handled by the early return at the top of this function.
            Some(DragKind::SettingsScrollbar) => false,
            None => false,
        }
    }

    /// Advance an edge-held preview drag by one line, driven by the event loop's ~33ms
    /// ticker rather than by mouse events, so holding the cursor at an edge scrolls
    /// continuously and grows the selection past a page. Returns whether anything moved.
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
        // Pace the scroll to a steady cadence regardless of how often the loop woke, so
        // the speed is even instead of racing capture-worker activity. Line scroll and the
        // wheel forward are fine-grained, so they run fast and read as smooth; the
        // no-mouse page-key fallback stays slow, since each press scrolls a whole page.
        const AUTOSCROLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(40);
        const PAGE_FORWARD_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);
        let now = std::time::Instant::now();
        let forward_interval = if self.preview_forwards_mouse().is_some() {
            AUTOSCROLL_INTERVAL
        } else {
            PAGE_FORWARD_INTERVAL
        };
        let line_ready = self
            .preview_autoscroll_at
            .is_none_or(|prev| now.duration_since(prev) >= AUTOSCROLL_INTERVAL);
        let forward_ready = self
            .preview_autoscroll_at
            .is_none_or(|prev| now.duration_since(prev) >= forward_interval);
        // First try the aoe-side capture-window scroll (normal-buffer panes
        // with real scrollback).
        if line_ready {
            let scrolled = if at_top {
                self.scroll_preview_offset(1)
            } else {
                self.scroll_preview_offset(-1)
            };
            if scrolled {
                self.preview_autoscroll_at = Some(now);
                let col_off = col.clamp(pane.x, pane.right().saturating_sub(1)) - pane.x;
                // Pin the extent to the revealed edge line in `from_bottom` terms, which
                // the new offset gives directly: the bottom visible line sits `offset`
                // lines up from the newest, the top `offset + height - 1`. Deriving it
                // from the offset rather than the stale `total_lines` keeps it correct
                // before the next capture.
                let offset = self.preview_scroll_offset as usize;
                let from_bottom = if at_top {
                    offset + (pane.height as usize).saturating_sub(1)
                } else {
                    offset
                };
                if let Some(sel) = self.preview_selection.as_mut() {
                    sel.extent = (col_off, from_bottom);
                }
                return true;
            }
        }
        // The capture-window scroll is inert for a full-screen agent, which has no
        // aoe-side scrollback, so forward what a wheel notch would send and scroll its own
        // transcript instead (#2421). The extent stays pinned to the screen edge; the
        // agent's redraw reveals more text under the held selection.
        if forward_ready && self.forward_scroll_to_preview(at_top, col, row) {
            self.preview_autoscroll_at = Some(now);
            return true;
        }
        false
    }

    /// Shift the preview scroll offset by `delta` lines (positive scrolls toward older
    /// output), clamped to the captured window, returning whether it moved. Factored out
    /// of the wheel handlers so the edge auto-scroll can move the pane without dragging
    /// the `handle_scroll_*` routing along.
    fn scroll_preview_offset(&mut self, delta: i32) -> bool {
        // Use the rendered output-body height the per-frame clamp uses
        // (`clamp_scroll_to_capture`), not `dimensions.1 - 1`, which over-counts the max
        // offset by a row: on an alternate-screen agent that phantom row oscillates
        // 0->1->0 every frame and, since it returns `true`, the caller never falls through
        // to the agent scroll-forward fallback.
        let visible_height = self.preview_visible_rows;
        let real_max = self
            .active_preview_cache()
            .captured_lines
            .saturating_sub(visible_height) as i32;
        let new = (self.preview_scroll_offset as i32 + delta).clamp(0, real_max) as u16;
        if new == self.preview_scroll_offset {
            return false;
        }
        self.preview_scroll_offset = new;
        true
    }

    /// End any active drag: the divider persists its final `list_width` to config, and a
    /// preview selection is marked `finalized` so the renderer keeps the highlight until
    /// dismissed. The clipboard copy is the caller's job (see `app.rs`), keeping this
    /// method free of side effects beyond state.
    ///
    /// True when a drag was in progress, so an `Up(Left)` that wasn't part of one causes
    /// no redraw.
    pub fn handle_drag_end(&mut self) -> bool {
        let Some(state) = self.drag_state.take() else {
            return false;
        };
        // The pointer is up, so edge auto-scroll stops; drop the tracked
        // position so a finalized highlight doesn't keep scrolling.
        self.preview_drag_pos = None;
        self.preview_autoscroll_at = None;
        match state {
            DragKind::ListDivider => {
                self.save_list_width();
            }
            DragKind::PreviewSelect => {
                // A bare click collapses anchor == extent; treat it as no selection so a
                // stray click doesn't paint a 1x1 highlight or copy one character. A real
                // drag is finalized, and the next render captures the cells so the app
                // loop can write them to the clipboard.
                if let Some(sel) = self.preview_selection {
                    if sel.anchor == sel.extent {
                        self.preview_selection = None;
                    } else if let Some(s) = self.preview_selection.as_mut() {
                        s.finalized = true;
                        self.preview_copy_pending = true;
                    }
                }
            }
            // The offset was applied live on each move; the release just
            // ends the gesture. Nothing to persist (scroll is view state).
            DragKind::SettingsScrollbar => {}
        }
        true
    }

    /// Whether `drag_state` is a PreviewSelect rather than a divider drag or nothing. The
    /// Down(Left) handler in `app.rs` uses it to tell which `handle_drag_start` installed.
    pub fn is_preview_select_dragging(&self) -> bool {
        matches!(self.drag_state, Some(DragKind::PreviewSelect))
    }

    /// Join the text under the current preview selection into a tmux-style flow string,
    /// called from `paint_preview_selection` on the render after `handle_drag_end`.
    ///
    /// The selection is anchored to absolute scrollback lines, so this reads the active
    /// cache's parsed `Text` rather than the visible frame buffer: the buffer holds only
    /// the current page, while the cache holds the whole captured window. Each line is
    /// laid back out into a one-row buffer at the pane width so column slicing handles
    /// wide chars and truncation as the on-screen render did.
    pub(super) fn extract_preview_selection_text(&self) -> Option<String> {
        let sel = self.preview_selection?;
        let view = self.preview_text_view;
        let width = view.pane.width;
        if width == 0 || view.total_lines == 0 {
            return None;
        }
        // The structured preview's transcript is its own line source: the view re-derives
        // the pre-wrapped rows the renderer painted (`wrapped_transcript`), so slicing
        // maps one-to-one onto the on-screen cells. Terminal previews read the tmux cache.
        let structured_lines = self
            .structured_preview
            .as_ref()
            .filter(|v| {
                self.selected_session
                    .as_deref()
                    .is_some_and(|id| id == v.session_id())
            })
            .map(|v| v.selection_text(width));
        let lines = match structured_lines.as_ref() {
            Some(text) => text,
            None => self.active_preview_cache().parsed_text.as_ref()?,
        };
        // Resolve `from_bottom` distances against the same `total_lines` the renderer used
        // this frame, so the copied range matches the painted highlight cell for cell.
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
                    // Trim only trailing whitespace: a selection over indented code keeps
                    // its indentation, while right-edge padding doesn't bloat the paste.
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

    /// Drain the text captured on the last render that painted a finalized preview
    /// selection. `Some` exactly once per finalized drag; `App` calls it after the draw.
    pub fn take_preview_copy_text(&mut self) -> Option<String> {
        self.preview_copy_text.take()
    }

    /// Discard any in-flight preview selection, so interacting with the TUI dismisses the
    /// highlight. True when state changed, so the caller can redraw.
    pub fn clear_preview_selection(&mut self) -> bool {
        if self.preview_selection.take().is_some() {
            // Cancel any in-progress drag too, so the next Up(Left) can't re-finalize a
            // stale selection and the edge auto-scroll stops chasing a cleared one.
            if matches!(self.drag_state, Some(DragKind::PreviewSelect)) {
                self.drag_state = None;
            }
            self.preview_drag_pos = None;
            self.preview_autoscroll_at = None;
            // A pending capture from an earlier finalized drag is moot once the selection
            // is gone.
            self.preview_copy_pending = false;
            self.preview_copy_text = None;
            true
        } else {
            false
        }
    }

    /// Dispatch the Submit branch of a confirm dialog. `Some(Action)` for confirm actions
    /// that emit a TUI action (`stop_session`, `quit_during_creation`); side-effect-only
    /// actions run inline and return `None`. Shared by the keyboard Enter path and the
    /// mouse-click path so both produce the same end state.
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
            "stop_terminal" => {
                if let Some((session_id, mode)) = self.pending_stop_terminal.take() {
                    if let Err(e) = self.kill_terminal_for(&session_id, mode) {
                        tracing::error!(target: "tui.input", "Failed to kill terminal: {}", e);
                    }
                }
                None
            }
            "stop_tool" => {
                if let Some((session_id, tool_name)) = self.pending_stop_tool.take() {
                    if let Err(e) = self.kill_tool_for(&session_id, &tool_name) {
                        tracing::error!(target: "tui.input", "Failed to kill tool session: {}", e);
                    }
                }
                None
            }
            "force_remove_session" => {
                if let Some(session_id) = self.pending_force_remove_session.take() {
                    if let Err(e) = self.force_remove_session(&session_id) {
                        tracing::error!(target: "tui.input", "Failed to force remove session: {}", e);
                    }
                }
                None
            }
            "trash_session" => {
                if let Some(session_id) = self.pending_trash_session.take() {
                    self.trash_session_by_id(&session_id);
                }
                None
            }
            "empty_trash" => {
                self.empty_trash_all();
                None
            }
            "pull_sandbox_image" => self.pending_image_pull.take().map(Action::SpawnImagePull),
            "switch_view" => self
                .pending_switch_view_session
                .take()
                .map(Action::SwitchSessionView),
            "start_daemon_structured" => self
                .pending_daemon_start_session
                .take()
                .map(Action::StartDaemonThenOpenStructured),
            "quit_during_creation" => Some(Action::Quit),
            "quit" => Some(Action::Quit),
            _ => None,
        }
    }

    /// Offer to pull the newer sandbox image the registry check surfaced. The image is
    /// stashed in `pending_image_pull` because `ConfirmDialog` carries only an action
    /// string; the Submit handler reads it back.
    pub(crate) fn prompt_pull_sandbox_image(&mut self, image: String) {
        if self.confirm_dialog.is_some() {
            return;
        }
        self.pending_image_pull = Some(image.clone());
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                "Update sandbox image",
                &format!(
                    "Pull the latest {image}? This downloads the new image and uses it for new sandbox sessions."
                ),
                "pull_sandbox_image",
            )
            .neutral(),
        );
    }

    /// Confirm before permanently purging every trashed session. The purge is
    /// irreversible, so it keeps the destructive red tone; an already-empty trash gets an
    /// info dialog instead of a confirm that would delete nothing.
    pub(super) fn prompt_empty_trash(&mut self) {
        let count = self.instances.values().filter(|i| i.is_trashed()).count();
        if count == 0 {
            self.info_dialog = Some(InfoDialog::new(
                "Trash is empty",
                "There are no trashed sessions to delete.",
            ));
            return;
        }
        let noun = if count == 1 { "session" } else { "sessions" };
        self.confirm_dialog = Some(ConfirmDialog::new(
            "Empty Trash",
            &format!("Permanently delete {count} trashed {noun}? This cannot be undone."),
            "empty_trash",
        ));
    }

    /// Confirm before archiving every active session under the focused group: a whole
    /// project at once is a bigger hammer than a single-row `z`. Archiving is reversible,
    /// hence the neutral tone. Silent no-op when the group has nothing active left.
    pub(super) fn prompt_archive_selected_group(&mut self) {
        let Some(group_path) = self.selected_group.clone() else {
            return;
        };
        let count = self.active_sessions_in_selected_group().len();
        if count == 0 {
            return;
        }
        // Project/Org mode groups by repo or owner and Manual mode by assigned path, so
        // name the scope accordingly and show the full path, or nested groups sharing a
        // leaf segment would be ambiguous.
        let (title, scope) = match self.group_by {
            crate::session::config::GroupByMode::Project => ("Archive project", "project"),
            crate::session::config::GroupByMode::Org => ("Archive org", "org"),
            crate::session::config::GroupByMode::Manual => ("Archive group", "group"),
        };
        let noun = if count == 1 { "session" } else { "sessions" };
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                title,
                &format!(
                    "Archive all {count} {noun} in {scope} \"{}\"?",
                    crate::session::project_group_display_name(&group_path)
                ),
                "archive_group",
            )
            .neutral(),
        );
    }

    /// Discard unsaved Settings changes and restore the saved config theme.
    /// Both keyboard confirmation and clicking `[Yes]` must undo live previews.
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
                        // No pending_intro_theme: the live preview already applied
                        // the chosen theme, and re-applying would force a
                        // `clear_terminal` close-flash. Same as the keyboard Submit.
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
            // A click inside the dialog that missed every hit rect (title, border) is
            // swallowed so the list underneath doesn't shift selection.
            return true;
        }
        if let Some(dialog) = &mut self.tips_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.tips_dialog = None;
                    }
                    DialogResult::Submit(outcome) => {
                        self.tips_dialog = None;
                        self.persist_tips_outcome(outcome);
                    }
                }
            }
            // Swallow every click while the overlay is open so it can't fall
            // through to the list underneath.
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
                        // Defensive: the new-session dialog submits only from the
                        // Enter path, while clicks return Continue.
                    }
                }
            }
            // Always swallow clicks while the new-session dialog is
            // open so the underlying list / preview don't react.
            return true;
        }
        // The confirm dialog floats over settings, so it wins click routing the same way
        // the keyboard path checks `settings_close_confirm` before `settings_view`;
        // otherwise a click on Yes / No goes into settings and never reaches the modal.
        if let Some(dialog) = &self.confirm_dialog {
            if let Some(result) = dialog.handle_click(col, row) {
                let action = dialog.action().to_string();
                match result {
                    DialogResult::Continue => {}
                    DialogResult::Cancel => {
                        self.confirm_dialog = None;
                        self.pending_stop_session = None;
                        self.pending_stop_terminal = None;
                        self.pending_stop_tool = None;
                        self.pending_force_remove_session = None;
                        self.pending_trash_session = None;
                        self.pending_image_pull = None;
                        // Mirrors the keyboard route: Cancel means "don't discard", so
                        // settings stays open and `settings_close_confirm` resets.
                        self.settings_close_confirm = false;
                    }
                    DialogResult::Submit(()) => {
                        let dont_ask_again = dialog.dont_ask_again();
                        self.confirm_dialog = None;
                        if self.settings_close_confirm {
                            // Discard runs the keyboard path's exact sequence, theme
                            // revert included; without it a mouse discard stranded a live
                            // theme preview until the next restart.
                            self.pending_dialog_click_action =
                                Some(self.discard_settings_changes());
                        } else {
                            if dont_ask_again {
                                self.apply_confirm_dont_ask_again(&action);
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
            // A press on the fields-panel scrollbar begins a grab-drag: seed the drag
            // state and jump to the pressed row so later Drag(Left) events map to the bar.
            // Must precede handle_click so the bar column isn't a field miss; assigning
            // `drag_state` ends the `view` borrow, hence the split into hit test and
            // follow-up.
            if view.hit_scrollbar(col, row) {
                view.scrollbar_drag_to_row(row);
                self.drag_state = Some(DragKind::SettingsScrollbar);
                return true;
            }
            // Settings is a full-screen takeover, so every click inside the area is for
            // it, hit or miss; `handle_click` mutates focus on hits and returns None
            // otherwise, and the click is swallowed either way.
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
                        crate::tmux::invalidate_agent_availability();
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
                    self.pending_dialog_click_action = self.activate_tool(tool_name, false);
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
            // The rename dialog's click handler only returns Continue (submitting needs
            // Enter on a valid input), so always swallow the click.
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
        if let Some(dialog) = &mut self.attach_project_dialog {
            match dialog.handle_click(col, row) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.attach_project_dialog = None;
                }
                DialogResult::Submit(project) => {
                    let id = dialog.session_id().to_string();
                    self.attach_project_dialog = None;
                    self.finish_add_project(&id, &project);
                }
            }
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
                    self.apply_sort_order(order);
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
                    // No `update_info` here: the mouse handler doesn't thread it
                    // through, and the palette commands that use it are
                    // keyboard-only, so the fallback is harmless.
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
                        match crate::session::config::update_app_state(|state| {
                            state.has_acknowledged_agent_hooks = true;
                        }) {
                            Ok(()) => {
                                self.hooks_install_dialog = None;
                                if let Some(data) = self.pending_hooks_install_data.take() {
                                    self.pending_dialog_click_action =
                                        self.maybe_confirm_volume_ignores_globs(data);
                                }
                            }
                            Err(e) => {
                                tracing::warn!(target: "tui.input", "Failed to save config: {e}")
                            }
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
                                    // Abort creation if trust cannot be persisted:
                                    // launching anyway leaves hooks treated as approved
                                    // while project MCP stays gated off the unwritten
                                    // hashes.
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
        // Other dialogs swallow clicks through the `has_dialog()` gates in the list,
        // preview and divider handlers.
        false
    }

    pub fn handle_key(
        &mut self,
        key: KeyEvent,
        update_info: Option<&crate::update::UpdateInfo>,
    ) -> Option<Action> {
        // Any keystroke drops a finalized preview selection: the highlight pins to cell
        // coords, so once the user does anything else the cells underneath can change and
        // it would point at unrelated content. Covers the live-send branch below too.
        self.clear_preview_selection();

        // Live-send capture normally wins over every other key handler: the home view is
        // a thin relay to the target pane, so dialog hotkeys, search and navigation
        // suspend until Ctrl+q. That holds while dialogs are keyboard-only, since
        // live-send swallows the hotkey. Once a non-live-send overlay is open (an
        // empty-sidebar click, a right-click menu), its keys must go to the overlay, or
        // the user sees a dialog whose Esc / Enter land on the session behind it.
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
                    self.refresh_from_config(crate::tui::home::ConfigRefreshOrigin::Interactive);
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
        if let Some(ref mut serve) = self.serve_view {
            match serve.handle_key(key) {
                ServeAction::Continue => return None,
                ServeAction::Close => {
                    self.serve_view = None;
                    return None;
                }
            }
        }

        // The right-click context menu routes before every other dialog so keys go to the
        // popup just opened. Submit dispatches through the shared helper, keeping the
        // keyboard and mouse paths aligned.
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
                    crate::tmux::invalidate_agent_availability();
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

        // Intro and changelog dialogs take priority. Intro live-previews themes as the
        // cursor moves, so drain any queued preview and emit it as `Action::SetTheme`,
        // letting the root App switch without round-tripping through settings.
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
                    // No SetTheme dispatch: the live preview already applied the theme
                    // on the picker page, and re-dispatching would re-trigger
                    // `set_theme -> needs_redraw`, forcing the close-flash
                    // `clear_terminal` on the next loop iteration.
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

        if let Some(dialog) = &mut self.tips_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.tips_dialog = None;
                }
                DialogResult::Submit(outcome) => {
                    self.tips_dialog = None;
                    self.persist_tips_outcome(outcome);
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
                    return self.activate_tool(tool_name, false);
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
                    match crate::session::config::update_app_state(|state| {
                        state.has_acknowledged_agent_hooks = true;
                    }) {
                        Ok(()) => {
                            self.hooks_install_dialog = None;
                            if let Some(data) = self.pending_hooks_install_data.take() {
                                return self.maybe_confirm_volume_ignores_globs(data);
                            }
                        }
                        Err(e) => tracing::warn!(target: "tui.input", "Failed to save config: {e}"),
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
                                // Abort creation if trust cannot be persisted, to avoid
                                // hooks approved while project MCP stays gated off.
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
                        // Backing out of `n` with a selection is the most contextual
                        // moment for the new-from-selection tip; queue it (a no-op until
                        // earned). Submit skips the pop so creation isn't interrupted and
                        // the badge still carries it. See #2262.
                        self.queue_earned_tip_pop();
                    }
                }
                DialogResult::Submit(data) => {
                    // Check if the tool uses hooks and user hasn't acknowledged yet
                    let tool_name = if data.tool.is_empty() {
                        "claude".to_string()
                    } else {
                        data.tool.clone()
                    };

                    let resolved_config = crate::session::resolve_config_with_repo_or_warn(
                        &data.profile,
                        std::path::Path::new(&data.path),
                    );
                    if let Some(hook_agent) =
                        resolve_hook_install_agent(&tool_name, &resolved_config.session)
                    {
                        let config = crate::session::config::load_config().ok().flatten();
                        let hooks_enabled = resolved_config.session.agent_status_hooks;
                        let acknowledged = config
                            .as_ref()
                            .map(|c| c.app_state.has_acknowledged_agent_hooks)
                            .unwrap_or(false);

                        if crate::agents::hook_install_required(hook_agent, hooks_enabled)
                            && !acknowledged
                        {
                            self.hooks_install_dialog =
                                Some(HooksInstallDialog::new_for_profile_resolved(
                                    &tool_name,
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
                    self.pending_stop_terminal = None;
                    self.pending_stop_tool = None;
                    self.pending_force_remove_session = None;
                    self.pending_trash_session = None;
                    self.pending_image_pull = None;
                }
                DialogResult::Submit(_) => {
                    let action = dialog.action().to_string();
                    let dont_ask_again = dialog.dont_ask_again();
                    self.confirm_dialog = None;
                    if dont_ask_again {
                        self.apply_confirm_dont_ask_again(&action);
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
                                data.rename_branch,
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
                        // Surface the restart error in an InfoDialog, not just the log:
                        // the user initiated this and needs to know it failed.
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

        if let Some(dialog) = &mut self.plugin_manager_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel | DialogResult::Submit(()) => {
                    self.plugin_manager_dialog = None;
                }
            }
            return None;
        }

        if let Some(dialog) = &mut self.skills_manager_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel | DialogResult::Submit(()) => {
                    self.skills_manager_dialog = None;
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

        if let Some(dialog) = &mut self.attach_project_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.attach_project_dialog = None;
                }
                DialogResult::Submit(project) => {
                    let id = dialog.session_id().to_string();
                    self.attach_project_dialog = None;
                    self.finish_add_project(&id, &project);
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
                    self.apply_sort_order(order);
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
                                self.rewire_after_profile_delete(&name);
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
                        // Defer the work to execute_action so the loop can render a
                        // status indicator first: the send path may start a container or
                        // wait out an agent splash, which inline would freeze the TUI.
                        return Some(Action::SendMessage(session_id, message));
                    }
                }
            }
            return None;
        }

        // Permission response dialog
        if let Some(dialog) = &mut self.permission_response_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.permission_response_dialog = None;
                    self.pending_permission_response = None;
                }
                DialogResult::Submit(choice) => {
                    self.permission_response_dialog = None;
                    if let Some(target) = self.pending_permission_response.take() {
                        match target {
                            PermissionResponseTarget::Terminal(session_id) => {
                                self.execute_permission_response(&session_id, choice);
                            }
                            PermissionResponseTarget::Structured { session_id, nonce } => {
                                self.resolve_structured_approval(session_id, nonce, choice);
                            }
                        }
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

        // Drain a queued earned-tip pop now that the home view is idle: every
        // overlay-routing block above has returned. Skipped while searching so it can't
        // interrupt a query, and opening it consumes the keystroke. #2262
        if !self.search_active && self.pending_tip_pop.is_some() && self.drain_pending_tip_pop() {
            return None;
        }

        // Search mode takes priority over the Ctrl+K palette binding below: while the
        // search input is focused every key feeds the box, and Esc exits first. Moving
        // this past the Ctrl+K check would let palette activation clobber search input.
        if self.search_active {
            match key.code {
                KeyCode::Esc => {
                    self.search_active = false;
                    self.search_query = Input::default();
                    self.search_matches.clear();
                    self.search_match_index = 0;
                }
                KeyCode::Enter => {
                    // vim-parity: Enter commits but keeps `search_matches` and
                    // `search_query` so `n`/`N` survive reloads
                    // (`refresh_search_matches` re-scores the same query). Esc above is
                    // the cancel-and-clear path. See #2676.
                    self.search_active = false;
                }
                _ => {
                    self.search_query
                        .handle_event(&crossterm::event::Event::Key(key));
                    self.update_search();
                }
            }
            return None;
        }

        // Ctrl+K opens the command palette in any hotkey mode, activated before strict
        // normalization so the binding stays discoverable on every keymap.
        if matches!(key.code, KeyCode::Char('k') | KeyCode::Char('K'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.open_command_palette();
            return None;
        }

        // Strict-mode relocation happens inside dispatch_action_key through the shared
        // bindings registry, so no key rewriting happens here.
        self.dispatch_action_key(key, update_info)
    }

    /// Whether the bottom search bar renders: while typing, or while a committed query
    /// is still set, so a zero-result `/xyzzy` keeps showing `/xyzzy [0/0]` instead of
    /// vanishing. Gated on the query rather than `search_matches`, and every search-exit
    /// path clears `search_query`. Reserves a list row in both states so the bar never
    /// overlaps the last session row.
    pub(super) fn search_bar_visible(&self) -> bool {
        self.search_active || !self.search_query.value().is_empty()
    }

    /// Run the main action dispatch on a key.
    ///
    /// Extracted from `handle_key` so the command palette routes through the same path.
    /// Relocatable action keys resolve through the shared [`bindings`](super::bindings)
    /// registry, so the dispatcher, palette and help overlay cannot drift on which chord
    /// an action binds to. Pure navigation keys, which never relocate, stay as explicit
    /// arms tried after the registry, and the strict-mode typing guard is the fallback.
    fn dispatch_action_key(
        &mut self,
        key: KeyEvent,
        update_info: Option<&crate::update::UpdateInfo>,
    ) -> Option<Action> {
        // Dynamic tool session hotkeys (checked before everything else).
        if let Some(tool_name) = self.match_tool_hotkey(&key) {
            return self.activate_tool(tool_name, true);
        }

        // Context-dependent Esc handling (not a relocatable action).
        match key.code {
            // Esc clears a committed search (the input box is already closed). Gated on
            // the query, not `search_matches`, so a zero-result committed search is still
            // dismissable rather than stuck on screen.
            KeyCode::Esc if !self.search_query.value().is_empty() => {
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
        match bindings::resolve_action(&key, self.strict_hotkeys, &ctx) {
            Some(bindings::ResolvedAction::Core(id)) => return self.run_action(id, update_info),
            Some(bindings::ResolvedAction::Plugin(action)) => {
                // Tier 0 has no plugin executor; the binding resolves and is
                // inspectable, but running it waits for the runtime host (#2095).
                self.info_dialog = Some(InfoDialog::sized_to_fit(
                    "Plugin action",
                    &format!(
                        "{} is a plugin action. Running plugin actions needs the plugin runtime, \
                         which is not available yet.",
                        action.canonical()
                    ),
                ));
                return None;
            }
            None => {}
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
            // Tab is the activation key's complement: whichever of live-send and tmux
            // attach `Enter` doesn't do. Only fires when `live_send` is None.
            KeyCode::Tab => {
                // A structured session has no tmux pane to attach or live-send into, so
                // both Tab meanings are vacuous; say so instead of ignoring the press.
                if let Some(id) = self.selected_session.as_deref() {
                    if self.get_instance(id).is_some_and(|i| i.is_structured()) {
                        return Some(Action::SetTransientStatus(
                            "Structured sessions have no tmux pane; press Enter to open the structured view."
                                .to_string(),
                        ));
                    }
                }
                let swap_to_attach = self
                    .selected_session
                    .as_deref()
                    .map(|id| {
                        matches!(
                            self.default_attach_mode(id),
                            Some(crate::session::AttachMode::LiveSend)
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
            // Strict-mode typing guard: a bare lowercase letter bound to nothing opens
            // the compose dialog pre-filled with that character (the
            // no-destructive-lowercase contract).
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

    /// Execute a resolved [`ActionId`], the single home for each action's behavior, so
    /// the keyboard dispatcher and the command palette cannot diverge.
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
                // Committed matches from a prior `/`-search persist across `Enter`
                // (#2676); clear them here so `[i/N]` doesn't render over an empty input
                // on reopen.
                self.search_matches.clear();
                self.search_match_index = 0;
            }
            ActionId::SearchNext => {
                if self.search_matches.is_empty() {
                    return None;
                }
                self.search_match_index = (self.search_match_index + 1) % self.search_matches.len();
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
                if matches!(self.view_mode, ViewMode::Terminal) {
                    if let Some(action) = self.maybe_auto_start_live_send() {
                        return Some(action);
                    }
                }
            }
            ActionId::SendMessage => {
                if let Some(id) = self.selected_structured_session() {
                    // Drain pending_paste into the structured composer so buffered
                    // paste or dictation is not lost when the legacy dialog is bypassed.
                    // Drafts are keyed by target session, so a second paste appends and a
                    // draft for another target stays put.
                    if let Some(buf) = self.pending_paste.take() {
                        self.pending_paste_for_structured_view
                            .entry(id.clone())
                            .or_default()
                            .push_str(&buf);
                    }
                    self.exit_live_send_if_active();
                    return Some(Action::OpenStructuredView(id));
                }
                self.open_send_message_dialog()
            }
            ActionId::RespondToPermission => self.open_permission_response_dialog(),
            ActionId::Stop => self.stop_selected(),
            ActionId::Delete => self.open_delete_for_selected(),
            ActionId::Rename => self.open_rename_for_selected(),
            ActionId::SetWorktreeName => self.open_worktree_name_for_selected(),
            ActionId::AddProject => self.open_add_project_for_selected(),
            ActionId::Diff => self.open_diff_for_selected(),
            ActionId::Serve => self.open_serve(),
            ActionId::Settings => self.open_settings(),
            ActionId::Profiles => self.show_profile_picker(),
            ActionId::Projects => {
                let profile = self.config_profile();
                self.projects_dialog = Some(ProjectsDialog::new(&profile));
            }
            ActionId::Plugins => {
                self.plugin_manager_dialog = Some(crate::tui::dialogs::PluginManagerDialog::new());
            }
            ActionId::Skills => {
                self.skills_manager_dialog = Some(crate::tui::dialogs::SkillsManagerDialog::new());
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
            ActionId::ToggleUnread => {
                if let Err(e) = self.toggle_unread_at_cursor() {
                    tracing::error!("toggle_unread_at_cursor failed: {}", e);
                }
            }
            ActionId::ToggleContainer => self.toggle_container_for_selected(),
            ActionId::TogglePreviewInfo => self.toggle_preview_info(),
            ActionId::ToggleDiagnostics => self.toggle_diagnostics(),
            ActionId::OpenSystemHealth => self.open_system_health(),
            ActionId::SortPicker => self.show_sort_picker(),
            ActionId::GroupBy => self.show_group_picker(),
            ActionId::ToggleProjectPin => self.toggle_project_pin_at_cursor(),
            ActionId::NextWaiting => self.jump_to_next_waiting(),
            ActionId::Tips => self.open_tips_dialog(),
            ActionId::Fork => self.open_fork_from_selection(),
            ActionId::AutoName => return self.auto_name_selected(),
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
        // Same gate as `open_new_session_dialog`: with no agent available the dialog has
        // nothing to create, so point at setup instead. Keeps `'N'` and the group menu's
        // New Session in step with `'n'` and the empty-sidebar menu.
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
                // No session selected (a project/group right-click, or `'N'` on a
                // header): borrow a member's repo path so the new session lands in the
                // same project, matching the web sidebar's per-project "+".
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
            .or_else(|| {
                // The scratch bucket's group_path is an internal sentinel; show
                // its display label in the Group field, not the raw sentinel (#3237).
                self.selected_group
                    .as_deref()
                    .map(|g| crate::session::project_group_display_name(g).to_string())
            });

        if prefill_path.is_some() || prefill_group.is_some() {
            let existing_groups: Vec<String> =
                self.all_groups().iter().map(|g| g.path.clone()).collect();
            let current_profile = self
                .profile_for_cursor(self.cursor)
                .unwrap_or_else(|| self.config_profile());
            let profiles =
                list_profiles_for_display().unwrap_or_else(|_| vec![current_profile.clone()]);
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
            // Skip to the title whenever the path is genuinely prefilled, inherited or
            // borrowed, so the user lands on naming. Only an empty group leaves focus on
            // the default cwd to be confirmed.
            if has_prefilled_path {
                dialog.focus_title();
            }
            self.new_dialog = Some(dialog);
            // The user just used N, so they've discovered it; suppress the tip
            // that teaches it.
            self.record_used_new_from_selection();
        }
    }

    /// Whether the session `id` can be forked, so the context menu shows "Fork session"
    /// only when the palette action would succeed: a structured parent needs an agent
    /// advertising the ACP fork capability, a terminal parent a forkable terminal agent.
    /// The captured-conversation precondition is deliberately not checked, so the row
    /// still shows for a not-yet-started session and the action explains "nothing to fork
    /// yet", as other rows do.
    pub(super) fn session_can_fork(&self, id: &str) -> bool {
        let Some(inst) = self.get_instance(id) else {
            return false;
        };
        if inst.is_structured() {
            crate::session::fork::structured_fork_capable(&inst.tool, inst.agent_name.as_deref())
        } else {
            inst.fork_parent_binding()
                .and_then(|parent| parent.execution.as_ref())
                .is_some_and(|execution| {
                    crate::session::fork::terminal_agent_can_fork(&execution.agent)
                })
        }
    }

    /// Whether the session's persisted view can be switched, and which it renders now:
    /// `Some(is_structured)` when the context menu should offer a switch. A structured
    /// session can always go back to a terminal; a terminal one can go structured only
    /// when its tool is ACP-capable and `offer_structured_in_new_session` is on, the same
    /// gate as the new-session toggle. Archived, trashed and mid-lifecycle rows are
    /// excluded: their agent is deliberately stopped and the swap would boot a worker
    /// behind the parked state.
    pub(super) fn session_switch_view_target(&self, id: &str) -> Option<bool> {
        let inst = self.get_instance(id)?;
        if inst.is_archived()
            || inst.is_trashed()
            || matches!(inst.status, Status::Creating | Status::Deleting)
        {
            return None;
        }
        if inst.is_structured() {
            return Some(true);
        }
        let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            &inst.source_profile,
            std::path::Path::new(&inst.project_path),
        );
        // Switching a terminal session into structured view is gated on the same opt-in
        // as the new-session toggle; the reverse direction stays available regardless, so
        // a session can always get out.
        (config.acp.offer_structured_in_new_session
            && crate::session::builder::structured::tool_acp_capable(&inst.tool, &config))
        .then_some(false)
    }

    /// Confirm before flipping the selected session's view: enabling structured view
    /// destroys the tmux scrollback, and disabling destroys the ACP transcript unless the
    /// pairing is CLI-resumable (claude), where the conversation continues via `--resume`
    /// and the copy says so (#2252). The id is stashed because `ConfirmDialog` carries
    /// only an action string.
    pub(super) fn prompt_switch_view_for_selected(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(to_structured) = self.session_switch_view_target(&id).map(|s| !s) else {
            return;
        };
        // Claude keeps context on the way back to the terminal (the switch resumes
        // `claude --resume`), so the warning must not threaten data loss for it. Mirrors
        // the backend gate (agents::acp_transcript_cli_resumable), approximated here by
        // agent_name or the tool. See #2252.
        let keeps_context = self.get_instance(&id).is_some_and(|inst| {
            let acp_agent = inst.agent_name.as_deref().unwrap_or(&inst.tool);
            crate::agents::acp_transcript_cli_resumable(&inst.tool, acp_agent)
        });
        let (title, body) = if to_structured && keeps_context {
            (
                "Switch to structured view",
                "Switch this session to the structured view? The tmux pane and its \
                 scrollback are cleared, but the conversation continues in structured \
                 view; the agent restarts under the aoe serve daemon (a local one is \
                 started if none is running).",
            )
        } else if to_structured {
            (
                "Switch to structured view",
                "Switch this session to the structured view? The tmux pane and its \
                 scrollback are destroyed; the agent restarts under the aoe serve \
                 daemon (a local one is started if none is running) with a fresh \
                 conversation.",
            )
        } else if keeps_context {
            (
                "Switch to terminal",
                "Switch this session back to a tmux terminal? The conversation \
                 continues in the terminal (the agent resumes it with \
                 `--resume`); the structured view is closed.",
            )
        } else {
            (
                "Switch to terminal",
                "Switch this session back to a tmux terminal? The structured \
                 conversation is closed; the agent restarts in a fresh terminal pane.",
            )
        };
        self.pending_switch_view_session = Some(id);
        self.confirm_dialog = Some(ConfirmDialog::new(title, body, "switch_view"));
    }

    /// Offer to start a localhost daemon when opening a structured view found none.
    /// Neutral tone: nothing is destroyed, the user is just about to run a background
    /// process. Remote setups keep their manual commands.
    pub(in crate::tui) fn prompt_start_daemon_for_structured(&mut self, session_id: &str) {
        self.pending_daemon_start_session = Some(session_id.to_string());
        self.confirm_dialog = Some(
            ConfirmDialog::new(
                "Start structured view daemon",
                "No `aoe serve` daemon is running, and structured sessions are \
                 driven by it. Start a local (localhost-only) daemon now? For \
                 remote access instead, cancel and run `aoe serve --daemon \
                 --remote` yourself.",
                "start_daemon_structured",
            )
            .neutral(),
        );
    }

    /// The selected session's id when it is a previewable structured row: not mid
    /// create/delete and not parked in archive or trash, which render their own
    /// placeholders. Drives preview-on-select and the active-view liveness check.
    pub(in crate::tui) fn selected_structured_session(&self) -> Option<String> {
        let id = self.selected_session.clone()?;
        let inst = self.get_instance(&id)?;
        let previewable = inst.is_structured()
            && !matches!(inst.status, Status::Creating | Status::Deleting)
            && !inst.is_archived()
            && !inst.is_trashed();
        previewable.then_some(id)
    }

    /// Leave live-send mode if active, restoring pane sizing. The embedded structured
    /// view opens through this: both modes route keystrokes away from the home view and
    /// paint the preview pane, so they cannot coexist.
    pub(in crate::tui) fn exit_live_send_if_active(&mut self) {
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
        }
    }

    /// Open the new-session dialog seeded as a fork of the selection: the fork resumes
    /// the parent's captured conversation under a fresh session id, inheriting its repo
    /// path and group like "new from selection". Refuses with an info dialog when the
    /// agent can't fork or nothing is captured yet.
    pub(super) fn open_fork_from_selection(&mut self) {
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

        // Copy the parent fields into owned locals so the immutable borrow of `self` ends
        // before the mutable calls below.
        let Some(parent) = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
        else {
            return;
        };
        let tool = parent.tool.clone();
        let parent_binding = parent.fork_parent_binding().cloned();
        let repo_path = if parent.is_structured() {
            parent.repo_path().to_string()
        } else {
            parent.project_path.clone()
        };
        let group_path = parent.group_path.clone();
        let title = parent.title.clone();
        let parent_is_structured = parent.is_structured();
        let parent_agent_name = parent.agent_name.clone();
        let parent_acp_session_id = parent.acp_session_id.clone();

        let seed = if parent_is_structured {
            // A structured parent forks via the ACP `session/fork` handshake rather than
            // the terminal resume-with-flag path, so the captured ACP session id is the
            // parent; without one there is nothing to fork.
            // Gated on the same predicate as the REST create-guard and the web
            // `acp_can_fork` projection: a resume-only ACP agent has no fork strategy, so
            // `session/fork` would be refused and silently downgrade to `session/new`,
            // handing back an empty session the user thinks is a fork.
            if !crate::session::fork::structured_fork_capable(&tool, parent_agent_name.as_deref()) {
                self.info_dialog = Some(InfoDialog::new(
                        "Fork not supported",
                        &format!(
                            "The '{}' agent cannot fork a structured view session. Fork is available for agents that support the ACP fork capability, such as Claude.",
                            tool
                        ),
                    ));
                return;
            }
            let Some(acp_id) = parent_acp_session_id.filter(|s| !s.is_empty()) else {
                self.info_dialog = Some(InfoDialog::new(
                        "Nothing to fork yet",
                        "This session has no captured conversation to fork from. Send it at least one message first.",
                    ));
                return;
            };
            crate::session::ForkSeed::Structured {
                parent_acp_session_id: acp_id,
            }
        } else {
            let child_id = crate::session::capture::generate_session_uuid();
            match crate::session::fork::terminal_fork_seed(parent_binding.as_ref(), child_id) {
                Ok(s) => s,
                Err(crate::session::ForkDenied::AgentCannotFork) => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Fork not supported",
                        &format!(
                            "The '{}' agent cannot fork a session. Fork is available for Claude, Codex, and OpenCode.",
                            tool
                        ),
                    ));
                    return;
                }
                Err(crate::session::ForkDenied::NoParentSession) => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Nothing to fork yet",
                        "This session has no captured conversation to fork from. Send it at least one message first.",
                    ));
                    return;
                }
            }
        };

        let existing_groups: Vec<String> =
            self.all_groups().iter().map(|g| g.path.clone()).collect();
        let current_profile = self
            .profile_for_cursor(self.cursor)
            .unwrap_or_else(|| self.config_profile());
        let profiles =
            list_profiles_for_display().unwrap_or_else(|_| vec![current_profile.clone()]);
        let mut dialog = NewSessionDialog::new(
            self.available_tools.clone(),
            existing_groups,
            &current_profile,
            profiles,
        );
        dialog.set_path(repo_path);
        if !group_path.is_empty() {
            dialog.set_group(group_path);
        }
        dialog.set_title(format!("{} (fork)", title));
        // The seed forks the parent's agent, so the dialog opens on that agent rather
        // than the configured default; otherwise a Codex parent would land on claude.
        dialog.set_tool(&tool);
        dialog.set_fork_from(seed);
        dialog.focus_title();
        self.new_dialog = Some(dialog);
    }

    /// A representative repo path for a selected group, so "New Session" can prefill the
    /// working directory. Project mode matches members by `project_group_key` (the label
    /// is a derived basename); manual mode by stored `group_path`, nested subgroups
    /// included. Org mode always returns `None`: an org spans many repos by design, as
    /// the web org header assumes. `None` too for an empty group, leaving the dialog on
    /// the default cwd, and for the synthetic scratch bucket, whose throwaway
    /// `<app_dir>/scratch/<id>` would tie a new session's cwd to another session's
    /// lifetime (#3237).
    pub(super) fn group_repo_path(&self, group_path: &str) -> Option<String> {
        if crate::session::is_synthetic_project_header(group_path) {
            return None;
        }
        self.instances
            .values()
            .find(|inst| match self.group_by {
                GroupByMode::Project => super::project_group_key(inst) == group_path,
                GroupByMode::Org => false,
                GroupByMode::Manual => {
                    inst.group_path == group_path
                        || inst.group_path.starts_with(&format!("{group_path}/"))
                }
            })
            .map(|inst| inst.repo_path().to_string())
            .or_else(|| {
                // An empty pinned project has no members to borrow from, so fall back to
                // the registered project's path; launching under it is the point of
                // pinning an empty project.
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

    pub(super) fn stop_selected(&mut self) {
        // Stop targets what the user is looking at: the paired terminal in Terminal view,
        // the tool session in Tool view. Only Agent view stops the agent session itself.
        match &self.view_mode {
            ViewMode::Terminal => {
                self.stop_terminal_selected();
                return;
            }
            ViewMode::Tool(tool_name) => {
                let tool_name = tool_name.clone();
                self.stop_tool_selected(&tool_name);
                return;
            }
            ViewMode::Structured => {}
        }
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

    /// Terminal-view Stop: confirm, then kill the paired terminal (host or container,
    /// whichever the row shows) without touching the agent session. No-op when the
    /// terminal isn't running, so Stop on an idle row pops no dialog.
    fn stop_terminal_selected(&mut self) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let Some(inst) = self.get_instance(&session_id) else {
            return;
        };
        let mode = self.effective_terminal_mode(&session_id);
        let terminal_running = match mode {
            TerminalMode::Container => inst
                .container_terminal_tmux_session()
                .map(|s| s.exists())
                .unwrap_or(false),
            TerminalMode::Host => inst
                .terminal_tmux_session()
                .map(|s| s.exists())
                .unwrap_or(false),
        };
        if !terminal_running {
            return;
        }
        let message = format!(
            "Are you sure you want to kill the terminal for '{}'?",
            inst.title
        );
        self.pending_stop_terminal = Some((session_id, mode));
        self.confirm_dialog = Some(ConfirmDialog::new(
            "Kill Terminal",
            &message,
            "stop_terminal",
        ));
    }

    /// Kill the paired terminal for `session_id` (host or container per `mode`) and
    /// refresh so the row drops back to its idle glyph. The agent session is untouched.
    pub(super) fn kill_terminal_for(
        &mut self,
        session_id: &str,
        mode: TerminalMode,
    ) -> anyhow::Result<()> {
        if let Some(inst) = self.get_instance(session_id) {
            match mode {
                TerminalMode::Container => inst.kill_container_terminal()?,
                TerminalMode::Host => inst.kill_terminal()?,
            }
        }
        crate::tmux::refresh_session_cache();
        self.reload()?;
        Ok(())
    }

    /// Tool-view Stop: confirm, then kill the tool session without touching the agent.
    /// Mirrors `stop_terminal_selected`, including the no-op when nothing runs.
    fn stop_tool_selected(&mut self, tool_name: &str) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let Some(inst) = self.get_instance(&session_id) else {
            return;
        };
        let tool_session = crate::tmux::ToolSession::new(&inst.id, &inst.title, tool_name);
        if !tool_session.exists() || tool_session.is_pane_dead() {
            return;
        }
        let message = format!(
            "Are you sure you want to kill {} for '{}'?",
            tool_name, inst.title
        );
        self.pending_stop_tool = Some((session_id, tool_name.to_string()));
        self.confirm_dialog = Some(ConfirmDialog::new("Kill Tool", &message, "stop_tool"));
    }

    /// Kill the tool session for `session_id`, then refresh so the Tool-view
    /// row drops back to its idle glyph. The agent session is left untouched.
    pub(super) fn kill_tool_for(
        &mut self,
        session_id: &str,
        tool_name: &str,
    ) -> anyhow::Result<()> {
        if let Some(inst) = self.get_instance(session_id) {
            let tool_session = crate::tmux::ToolSession::new(&inst.id, &inst.title, tool_name);
            if tool_session.exists() {
                tool_session.kill()?;
            }
        }
        crate::tmux::refresh_session_cache();
        self.reload()?;
        Ok(())
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

        // A session on a non-git project runs in place, so there is no repo to diff
        // against; say so rather than surfacing the git layer's raw error.
        if !crate::git::GitWorktree::is_git_repo(&repo_path) {
            self.info_dialog = Some(InfoDialog::new(
                "No Git Repository",
                "This session runs in place in a non-git directory, so there is no diff to show.",
            ));
            return;
        }

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

    /// "Auto-name now": run the agent one-shot title generator for the selected
    /// session, even when auto-rename-on-start is off (#3039) and over a chosen title.
    /// Terminal sessions rename via a detached child, structured ones through the daemon.
    /// Best-effort, not a synchronous rename.
    fn auto_name_selected(&mut self) -> Option<Action> {
        let Some(id) = self.selected_session.clone() else {
            self.info_dialog = Some(InfoDialog::new(
                "No Session Selected",
                "Select a session to auto-name it.",
            ));
            return None;
        };
        let Some((title, structured, profile, tool, command, project_path, sandboxed)) =
            self.get_instance(&id).map(|inst| {
                (
                    inst.title.clone(),
                    inst.is_structured(),
                    inst.source_profile.clone(),
                    inst.tool.clone(),
                    inst.command.clone(),
                    inst.project_path.clone(),
                    inst.is_sandboxed(),
                )
            })
        else {
            self.info_dialog = Some(InfoDialog::new("Error", "Could not find session data."));
            return None;
        };

        if structured {
            return Some(Action::SmartRenameNow(id));
        }

        // Preflight the gates the detached child re-applies, so this action stops
        // reporting "auto-naming" for a session the child will silently drop
        // (#3159). The child stays the authority; this is feedback and
        // fork avoidance. `setting_on` and `force` are true because the manual
        // action runs even when auto-rename-on-start is off (#3039) and over any
        // title, matching the `--force` the child receives and the web preflight.
        let resolved = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            &profile,
            std::path::Path::new(&project_path),
        );
        let cfg =
            crate::session::smart_rename::resolve_smart_rename_config(&resolved.session, None);
        if let Err(reason) = crate::session::smart_rename::check_eligible_resolved(
            true,
            true,
            true,
            &title,
            &tool,
            cfg.rename_agent,
            sandboxed,
            &command,
            cfg.overrides,
        ) {
            self.info_dialog = Some(InfoDialog::new("Can't Auto-Name", reason.user_message()));
            return None;
        }

        // A sandboxed session's one-shot runs inside its container, so a stopped container
        // means "not now" rather than "never". Only sandboxed sessions are inspected, so
        // the common path spawns no `docker inspect`.
        if sandboxed {
            use crate::containers::Probe;
            // A failed inspection is not a stopped container: telling the user to start
            // one that may already be running sends them the wrong way, so the daemon
            // error is surfaced verbatim.
            match crate::containers::DockerContainer::from_session_id(&id).probe_running() {
                Probe::Running => {}
                Probe::NotRunning => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Container Not Running",
                        "This session's sandbox container isn't running, so its agent can't be asked for a name. Open the session to start it, then try again.",
                    ));
                    return None;
                }
                Probe::Unknown(e) => {
                    self.info_dialog = Some(InfoDialog::new(
                        "Container State Unknown",
                        &format!(
                            "Couldn't check this session's sandbox container, so its agent can't be asked for a name: {e}"
                        ),
                    ));
                    return None;
                }
            }
        }

        crate::session::smart_rename::spawn_smart_rename_now(&profile, &id);
        // Transient status (not a modal), matching the structured path's
        // feedback so the two on-demand triggers behave the same.
        Some(Action::SetTransientStatus(format!(
            "auto-naming \"{title}\"…"
        )))
    }

    fn open_serve(&mut self) {
        let web_disabled = crate::plugin::registry()
            .get("aoe.web")
            .is_some_and(|p| !p.enabled);
        if web_disabled {
            self.info_dialog = Some(InfoDialog::new(
                "Web dashboard disabled",
                "The aoe.web plugin is disabled, so the web dashboard cannot \
                     be served.\n\n\
                     Re-enable it in Settings > Plugins (or run \
                     `aoe plugin enable aoe.web`), then press R again.",
            ));
            return;
        }
        self.serve_view = Some(crate::tui::dialogs::ServeView::new());
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
        let mut entries: Vec<PaletteCommand> = builtin_commands(self.strict_hotkeys);

        // Quit lives in the registry but is excluded from `builtin_commands` (no palette
        // metadata) so it can sit in the Settings group at the end; add it here, still
        // routed through the shared action dispatch.
        entries.push(PaletteCommand {
            id: bindings::palette_id(ActionId::Quit),
            title: "Quit Agent of Empires".to_string(),
            group: PaletteGroup::Settings,
            keywords: vec!["exit", "close"],
            hotkey: bindings::label(ActionId::Quit, self.strict_hotkeys),
            payload: PaletteAction::Invoke(ActionId::Quit),
        });

        // Dynamic session/group entries, one per flat_items row, so the user can
        // fuzzy-search and jump to it. In-flight sessions are tagged in the title so it is
        // clear that Stop/Delete will be a no-op for them.
        for (idx, item) in self.flat_items.iter().enumerate() {
            match item {
                Item::Session { id, .. } => {
                    let Some(inst) = self.get_instance(id) else {
                        continue;
                    };
                    let status_tag = if inst.is_shown_dormant() {
                        " [dormant]"
                    } else {
                        match inst.status {
                            Status::Creating => " [creating]",
                            Status::Deleting => " [deleting]",
                            Status::Stopped => " [stopped]",
                            _ => "",
                        }
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
                    // The synthetic Archived header (and its Project-mode sub-folders)
                    // is not a real group, so skip it: the palette must not surface the
                    // sentinel path or offer Jump-to-group navigation the rest of the
                    // codebase disarms.
                    if crate::session::is_within_archived_section(path)
                        || crate::session::is_within_trash_section(path)
                    {
                        continue;
                    }
                    // The parenthetical disambiguates org keys (owner vs owner@host);
                    // route it through the display mapper so a synthetic bucket's sentinel
                    // path never leaks (#3237).
                    let disambiguator = crate::session::project_group_display_name(path);
                    let label = if name.as_str() == disambiguator {
                        format!("Jump to group: {}", name)
                    } else {
                        format!("Jump to group: {} ({})", name, disambiguator)
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
            let title = if config.background {
                format!("Run: {}{}", name, hotkey_label)
            } else {
                format!("Open tool: {}{}", name, hotkey_label)
            };
            entries.push(PaletteCommand {
                id: "tool-session",
                title,
                group: PaletteGroup::Actions,
                keywords: vec!["tool", "session"],
                hotkey: String::new(),
                payload: PaletteAction::ToolSession(name.clone()),
            });
        }

        self.command_palette = Some(CommandPaletteDialog::new(entries));
    }

    /// Apply a palette pick: `Key` re-enters the action dispatch with the synthesized
    /// event (bypassing strict normalization, which the palette already accounts for),
    /// `JumpToCursor` moves the selection.
    fn dispatch_palette_action(
        &mut self,
        action: PaletteAction,
        update_info: Option<&crate::update::UpdateInfo>,
    ) -> Option<Action> {
        // The palette can be opened over live mode via the leader, but every command
        // steps out of the per-session relay: jumping navigates away and the others change
        // what is focused, while the preview follows `selected_session` and keystrokes
        // target `live_send`. Committing one while still live would desync the two, so
        // leave live mode first. Cancelling never reaches here, so Esc still returns to
        // live mode.
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
        }
        match action {
            PaletteAction::Invoke(id) => {
                // The palette's model is "run the named action", so clear leftover
                // search state first: otherwise "New session" while a search is committed
                // would route the dual-purpose `n`/`N` into a search-cycle. `search_query`
                // goes too (#2676), so a later `refresh_search_matches` cannot resurrect
                // phantom matches.
                self.search_matches.clear();
                self.search_match_index = 0;
                self.search_query = Input::default();
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
            PaletteAction::ToolSession(tool_name) => self.activate_tool(tool_name, false),
            PaletteAction::Cheat(message) => Some(Action::SetTransientStatus(message)),
        }
    }

    fn jump_to_session_id(&mut self, id: &str) {
        if let Some(idx) = self
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id: sid, .. } if sid == id))
        {
            self.cursor = idx;
            self.update_selected();
            return;
        }

        let previous = self.selected_session.clone();
        self.select_and_reveal_session(id);
        if self.selected_session != previous {
            self.preview_scroll_offset = 0;
            self.manual_unread_hold = None;
        }
    }

    fn jump_to_next_waiting(&mut self) {
        let len = self.flat_items.len();
        if len == 0 {
            return;
        }

        let visible_sessions: std::collections::HashSet<String> = self
            .flat_items
            .iter()
            .filter_map(|item| match item {
                Item::Session { id, .. } => Some(id.clone()),
                Item::Group { .. } => None,
            })
            .collect();
        let current_session = self.selected_session.clone();

        // Pass 1: forward-walk from cursor+1, wrapping, for the next Waiting session or a
        // freshly-stopped Idle one (within `idle_decay_window`). Both need attention, so
        // they cycle together and repeated `w` taps walk the actionable backlog.
        let window = self.idle_decay_window;
        let start = (self.cursor + 1) % len;
        for i in 0..len - 1 {
            let idx = (start + i) % len;
            let id = match self.flat_items.get(idx) {
                Some(Item::Session { id, .. }) => id.clone(),
                _ => continue,
            };
            if let Some(inst) = self.get_instance(&id) {
                // Trashed rows are stopped and only surface under the collapsed Trash
                // section, so they never need attention even if a stale unread flag
                // survived (#2489). Snoozed and archived rows are the same explicit
                // "don't bother me" sink states.
                let is_actionable = !inst.is_dismissed()
                    && (inst.status == Status::Waiting
                        || matches!(inst.idle_age(), Some(age) if age < window)
                        || (crate::session::unread_enabled() && inst.is_unread()));
                if is_actionable {
                    self.jump_to_session_id(&id);
                    return;
                }
            }
        }

        let hidden_actionable = self
            .instances
            .values()
            .find(|inst| {
                if visible_sessions.contains(&inst.id)
                    || current_session.as_deref() == Some(inst.id.as_str())
                    || inst.is_dismissed()
                {
                    return false;
                }
                inst.status == Status::Waiting
                    || matches!(inst.idle_age(), Some(age) if age < window)
                    || (crate::session::unread_enabled() && inst.is_unread())
            })
            .map(|inst| inst.id.clone());
        if let Some(id) = hidden_actionable {
            self.jump_to_session_id(&id);
            return;
        }

        // Pass 2: fall back to the next non-dismissed Idle session in list order, skipping
        // the cursor. Recomputing a global most-recently-accessed row here made older idle
        // sessions unreachable, with `w` toggling between the newest rows.
        for i in 0..len - 1 {
            let idx = (start + i) % len;
            let id = match self.flat_items.get(idx) {
                Some(Item::Session { id, .. }) => id.clone(),
                _ => continue,
            };
            let Some(inst) = self.get_instance(&id) else {
                continue;
            };
            if !inst.is_dismissed() && inst.status == Status::Idle {
                self.jump_to_session_id(&id);
                return;
            }
        }

        // A collapsed group may hide an idle session from `flat_items`. Keep
        // the existing most-recently-accessed selection for hidden rows.
        let mut best_hidden: Option<(String, Option<chrono::DateTime<chrono::Utc>>)> = None;
        for inst in self.instances.values() {
            if visible_sessions.contains(&inst.id)
                || current_session.as_deref() == Some(inst.id.as_str())
                || inst.is_dismissed()
                || inst.status != Status::Idle
            {
                continue;
            }
            let ts = inst.last_accessed_at;
            let beats = match best_hidden {
                None => true,
                Some((_, b)) => match (ts, b) {
                    (Some(a), Some(b)) => a > b,
                    (Some(_), None) => true,
                    (None, _) => false,
                },
            };
            if beats {
                best_hidden = Some((inst.id.clone(), ts));
            }
        }

        if let Some((id, _)) = best_hidden {
            self.jump_to_session_id(&id);
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
        // Keyboard nav overrides any prior hover: when a prediction layer eats the `Moved`
        // event that fires as the cursor leaves the list, the hover background stays
        // painted alongside the keyboard-selected row. handle_hover only clears
        // `mouse_pos` on an off-list Moved, so a keyboard transition must clear it here.
        self.mouse_pos = None;
        self.update_selected();
    }

    /// The action "activating" the selected session should produce (structured view
    /// open, tmux attach, tool attach). `None` for in-flight sessions and when nothing is
    /// selected. Shared by the `Enter` keybind and double-click so they cannot drift.
    pub(super) fn activate_selected_session(&mut self) -> Option<Action> {
        self.system_health_open = false;
        let id = self.selected_session.clone()?;
        if let Some(inst) = self.get_instance(&id) {
            if matches!(inst.status, Status::Deleting | Status::Creating) {
                return None;
            }
            if inst.is_structured() {
                // The embedded structured view takes over the preview pane, so leave
                // live-send first (mirrors `exit_live_send_before_attach`).
                self.exit_live_send_if_active();
                return Some(Action::OpenStructuredView(id));
            }
        }
        match self.view_mode {
            ViewMode::Structured => {
                // `default_attach_mode = LiveSend` swaps the historical tmux attach for
                // live-send on Enter / double-click. Terminal view honors the same
                // setting against the paired terminal pane; Tool view keeps
                // AttachToolSession.
                //
                // Routing through `start_live_send` honors the same-target guard, so a
                // double-click on the live row does not re-run ensure_pane_ready and
                // respawn the worker. It returns `None` for that and for structured or
                // creating rows, where activation is left alone.
                if matches!(
                    self.default_attach_mode(&id),
                    Some(crate::session::AttachMode::LiveSend)
                ) {
                    self.start_live_send()
                } else {
                    self.exit_live_send_before_attach();
                    Some(Action::AttachSession(id))
                }
            }
            ViewMode::Terminal => {
                // Mirror Structured view: under `default_attach_mode = LiveSend`, Enter
                // on a terminal row enters live-send against the paired pane (host or
                // container, whichever shows); otherwise fall back to tmux attach.
                if matches!(
                    self.default_attach_mode(&id),
                    Some(crate::session::AttachMode::LiveSend)
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
                self.exit_live_send_before_attach();
                Some(Action::AttachTerminal(id, terminal_mode))
            }
            ViewMode::Tool(ref tool_name) => {
                let tool_name = tool_name.clone();
                self.exit_live_send_before_attach();
                Some(Action::AttachToolSession(id, tool_name))
            }
        }
    }

    /// The "Tab swap" action under `default_attach_mode = LiveSend`: Enter takes the
    /// live-send slot, so Tab takes tmux attach. Mirrors the structured-view and
    /// in-flight guards from `activate_selected_session`.
    pub(super) fn tab_attach_action(&mut self) -> Option<Action> {
        let id = self.selected_session.clone()?;
        if let Some(inst) = self.get_instance(&id) {
            if matches!(inst.status, Status::Deleting | Status::Creating) {
                return None;
            }
            // Structured rows never reach here: the Tab keybinding refuses them with a
            // "no tmux pane" toast first.
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
                    if crate::session::is_within_archived_section(path)
                        || crate::session::is_within_trash_section(path)
                    {
                        // The synthetic Archived section (and its Project-mode
                        // sub-folders) is not a real group and cannot be renamed, deleted,
                        // archived or moved. Leaving `selected_group` unset disarms every
                        // keybind that branches on `selected_group.is_some()` without each
                        // one special-casing the sentinel.
                        self.selected_group = None;
                        self.selected_group_profile = None;
                    } else {
                        self.selected_group = Some(path.clone());
                        self.selected_group_profile = self.profile_for_cursor(self.cursor);
                    }
                }
            }
            if self.selected_session != prev_session {
                self.system_health_open = false;
                self.preview_scroll_offset = 0;
                // A finalized preview selection pins to the previous pane's cells, so
                // carrying it into another session would paint a stale highlight and, since
                // a live selection freezes the preview, stop the new session's output from
                // following. Keystroke and click navigation drop it upstream; this also
                // covers programmatic reselects.
                self.clear_preview_selection();
                // Moving off a hand-flagged row ends its manual-unread hold, so returning
                // later dwell-clears like any other unread. Done at the cursor->selection
                // sync, which every navigation path runs through, so the release doesn't
                // hinge on a dwell tick firing during a quick hop.
                self.manual_unread_hold = None;
            }
        }
    }

    /// Put the cursor back on `selected_session` after a `flat_items` rebuild. Mode flips
    /// reshape the list, so index-based clamping lands on whatever slid into the old slot;
    /// seeking by session id keeps focus on the row the user was looking at. Falls back to
    /// the clamp when there was no prior selection or the session left the flat list.
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
        if self.flat_items.is_empty() {
            self.cursor = 0;
            self.selected_session = None;
            self.selected_group = None;
            self.selected_group_profile = None;
            return;
        }
        self.cursor = self.cursor.min(self.flat_items.len().saturating_sub(1));
        self.update_selected();
    }

    pub(super) fn apply_sort_order(&mut self, new_order: SortOrder) {
        self.sort_order = new_order;
        if self.search_active && !self.search_query.value().is_empty() {
            self.flat_items = self.build_flat_items();
            self.update_search();
        } else {
            self.rebuild_flat_items();
            self.reseat_cursor_after_rebuild();
        }
        let sort_order = self.sort_order;
        if let Err(e) = update_app_state(|state| {
            state.sort_order = Some(sort_order);
        }) {
            tracing::warn!(target: "tui.input", "Failed to save sort order: {}", e);
        }
    }

    fn apply_group_by(&mut self, new_mode: GroupByMode) {
        self.group_by = new_mode;
        self.rebuild_flat_items();
        self.reseat_cursor_after_rebuild();
        let group_by = self.group_by;
        if let Err(e) = update_app_state(|state| {
            state.group_by = Some(group_by);
        }) {
            tracing::warn!(target: "tui.input", "Failed to save group_by mode: {}", e);
        }
    }

    /// Info-dialog copy for a rename/delete attempted on a header derived automatically
    /// (Project/Org), where there is no user-owned group to act on. `None` for `Manual`,
    /// where the caller proceeds with its normal flow.
    fn automatic_group_hint(&self) -> Option<(String, String)> {
        let mode_label = match self.group_by {
            GroupByMode::Manual => return None,
            GroupByMode::Project => "Project",
            GroupByMode::Org => "Org",
        };
        let toggle = if self.strict_hotkeys { "Ctrl+G" } else { "'g'" };
        Some((
            format!("Cannot Modify {mode_label} Groups"),
            format!(
                "{mode_label} groups are automatic. Press {toggle} and pick Manual to manage groups."
            ),
        ))
    }

    fn toggle_group_collapsed(&mut self, path: &str) {
        // The synthetic Archived section is not in any GroupTree; its collapsed state
        // lives on HomeView. Route here before either branch mutates a nonexistent group.
        if crate::session::is_archived_section_path(path) {
            self.toggle_archived_section();
            return;
        }
        if crate::session::is_trash_section_path(path) {
            self.toggle_trashed_section();
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
            self.rebuild_flat_items();
            self.save_project_group_collapsed();
            return;
        }
        if self.group_by == GroupByMode::Org {
            let collapsed = self.org_group_collapsed.get(path).copied().unwrap_or(false);
            self.org_group_collapsed
                .insert(path.to_string(), !collapsed);
            self.rebuild_flat_items();
            self.save_org_group_collapsed();
            return;
        }
        // Route to the correct profile's GroupTree
        let profile = self.profile_for_cursor(self.cursor);
        if let Some(profile) = profile {
            if let Some(tree) = self.group_trees.get_mut(&profile) {
                tree.toggle_collapsed(path);
            }
        }
        self.rebuild_flat_items();
        if let Err(e) = self.save() {
            tracing::error!(target: "tui.input", "Failed to save group state: {}", e);
        }
    }

    /// Forward one wheel notch to the previewed full-screen pane so it scrolls its own
    /// content, as a terminal does on direct attach. Active in live-send and passive
    /// preview alike: the alternate screen has no scrollback, so the capture-window scroll
    /// is inert there and forwarding is the only way to reach the agent's history.
    /// Branched on what the app asked for (see `wheel_forward_key`):
    ///
    /// * **Mouse tracking on**: forward as a mouse event, SGR 1006 or legacy X10 per
    ///   `mouse_sgr`. The pane is sized to the preview rect in both modes, so the mapped
    ///   coordinates land inside it.
    /// * **Mouse tracking off**: send `PageUp`/`PageDown`, not arrows, which a full-screen
    ///   app reads as cursor or history navigation (#2407).
    ///
    /// Normal-buffer panes get `None` from `wheel_forward_key`, leaving the caller's
    /// capture-window scroll, which reaches real scrollback there. In live-send the key
    /// rides the ordered `LiveSendWorker` to stay in sequence with typed keystrokes; in
    /// passive preview it goes out as a one-shot fork. True when forwarded.
    fn forward_wheel_to_preview(&self, up: bool, col: u16, row: u16) -> bool {
        let cursor = self.active_preview_cursor();
        let Some(cursor) = cursor else { return false };
        let Some(key) = wheel_forward_key(&cursor, up, self.preview_text_view.pane, col, row)
        else {
            return false;
        };
        self.send_to_preview_pane(key)
    }

    /// During an edge-held selection over a full-screen agent the capture-window scroll
    /// is inert, so forward what one wheel notch would send and scroll the agent's own
    /// transcript. Reuses `wheel_forward_key`, so a mouse-tracking app gets a wheel report
    /// at the held cell and a no-mouse app gets `PageUp`/`PageDown`, and its
    /// alternate-screen gate keeps scroll input out of a normal-buffer shell. `up` picks
    /// the direction; `col`/`row` is the held cell. True when something was sent.
    fn forward_scroll_to_preview(&self, up: bool, col: u16, row: u16) -> bool {
        let Some(cursor) = self.active_preview_cursor() else {
            return false;
        };
        let Some(key) = wheel_forward_key(&cursor, up, self.preview_text_view.pane, col, row)
        else {
            return false;
        };
        self.send_to_preview_pane(key)
    }

    /// Send a forwarded key or mouse-byte payload to the pane the preview shows
    /// (`preview_capture_target`), which is what the cursor and mapped coordinates
    /// describe. Routed through the ordered live-send worker only when that pane is also
    /// the live-send target, so it stays in sequence with typed keystrokes; otherwise a
    /// one-shot send is forked. True when something was dispatched.
    fn send_to_preview_pane(&self, key: live_send::TmuxKey) -> bool {
        let Some(target) = self.preview_capture_target.as_deref() else {
            return false;
        };
        if let (Some(worker), Some(live)) = (&self.live_send_worker, &self.live_send) {
            if live.tmux_name.as_str() == target {
                worker.send(key);
                return true;
            }
        }
        live_send::send_key_oneshot(target, key);
        true
    }

    /// The previewed agent's cursor when a mouse button event over the preview should go
    /// to it instead of driving aoe's UI: the pane must be a full-screen app with mouse
    /// tracking on, the same gate as the wheel's mouse-byte branch. Works in passive
    /// preview too, where `send_to_preview_pane` forks a one-shot send. `None` lets the
    /// event fall through to aoe's handlers; Shift is the caller's escape hatch. The
    /// cursor comes back so the caller can read `mouse_sgr` for the encoding.
    fn preview_forwards_mouse(&self) -> Option<crate::tmux::PaneCursor> {
        let cursor = self.active_preview_cursor()?;
        (cursor.alternate_on && cursor.mouse_tracking).then_some(cursor)
    }

    /// Forward a mouse button event (press / release / drag) over the preview to the
    /// previewed agent, as a direct tmux attach would. A Shift-held event returns `false`
    /// and falls through to aoe's own text-selection. Wheel and bare-motion events have
    /// their own paths (`forward_wheel_to_preview`, `forward_hover_to_preview`).
    ///
    /// A held button is tracked in `mouse_forward_btn` so its drag and release reach the
    /// agent even if the pointer leaves the preview rect, which keeps the agent from
    /// seeing a stuck button.
    pub fn forward_mouse_to_preview(
        &mut self,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
        col: u16,
        row: u16,
    ) -> bool {
        use crossterm::event::{MouseButton, MouseEventKind};
        let base = |b: MouseButton| match b {
            MouseButton::Left => 0u16,
            MouseButton::Middle => 1,
            MouseButton::Right => 2,
        };
        // Decide (button, release, motion) and whether this event even starts
        // or continues a forwarded gesture.
        let (base_button, release, motion) = match kind {
            MouseEventKind::Down(b) => {
                if modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
                    return false; // Shift+press => aoe selection
                }
                // A modal over the preview owns the click (dismiss / button).
                if self.has_non_live_send_overlay() {
                    return false;
                }
                if !self.hit_preview(col, row) {
                    return false;
                }
                (base(b), false, false)
            }
            // A drag / release only forwards if its press was forwarded (we're
            // mid-gesture); position is no longer gated so the release lands.
            MouseEventKind::Drag(b) if self.mouse_forward_btn.is_some() => (base(b), false, true),
            MouseEventKind::Up(b) if self.mouse_forward_btn.is_some() => (base(b), true, false),
            _ => return false,
        };
        // The single mouse-forwarding gate shared by press/drag/release: a press over a
        // non-mouse pane bails here, and a gesture whose pane stopped being a mouse agent
        // mid-drag drops its tracked button rather than stranding it.
        let Some(cursor) = self.preview_forwards_mouse() else {
            self.mouse_forward_btn = None;
            return false;
        };
        // On a composited preview only pane 0 takes input, so a press outside it must not
        // open a gesture: forwarding would report the click at a clamped cell pane 0 never
        // saw. Drags and releases stay ungated so a gesture begun on pane 0 completes.
        let press = !release && !motion;
        if press && mouse_target_rect(&cursor, self.preview_text_view.pane, col, row).is_none() {
            self.mouse_forward_btn = None;
            return false;
        }
        self.mouse_forward_btn = if release { None } else { Some(base_button) };
        let bytes = mouse_event_bytes(
            base_button,
            release,
            motion,
            cursor.mouse_sgr,
            mouse_pane_rect(&cursor, self.preview_text_view.pane),
            col,
            row,
        );
        self.send_to_preview_pane(live_send::TmuxKey::HexBytes(bytes))
    }

    /// Forward a bare mouse-motion event to a previewed agent in any-event tracking (DEC
    /// 1003), so its hover-driven UI works in the preview as it does over a direct attach.
    /// Active in live-send and passive preview. Deduped per mapped pane cell, since
    /// crossterm can re-report a cell and one report per cell crossed is what a real
    /// terminal delivers. Unlike `forward_mouse_to_preview` it never consumes the event,
    /// so aoe's own hover handling still runs. True when a report was sent.
    pub fn forward_hover_to_preview(&mut self, col: u16, row: u16) -> bool {
        if self.has_non_live_send_overlay() || !self.hit_preview(col, row) {
            // Off-preview (or covered by a modal): drop the dedup cell so
            // re-entering the preview on the same cell reports again.
            self.hover_forward_cell = None;
            return false;
        }
        let Some(cursor) = self.active_preview_cursor() else {
            return false;
        };
        let pane = self.preview_text_view.pane;
        let Some(bytes) = hover_forward_bytes(&cursor, pane, col, row) else {
            return false;
        };
        let cell = map_pane_cell(pane, col, row);
        if self.hover_forward_cell == Some(cell) {
            return false;
        }
        self.hover_forward_cell = Some(cell);
        self.send_to_preview_pane(live_send::TmuxKey::HexBytes(bytes))
    }

    pub fn handle_scroll_up(&mut self, col: u16, row: u16) -> bool {
        const STEP: u16 = 3;
        // Settings is a full-screen takeover with its own scrollable fields panel, so the
        // wheel drives it wherever the now-hidden list/preview rects were. Checked before
        // the routing below, which would swallow the wheel via `has_dialog()`.
        if let Some(view) = &mut self.settings_view {
            return view.handle_wheel_scroll(true);
        }
        if self.system_health_open && self.hit_preview(col, row) {
            self.system_health_scroll = self.system_health_scroll.saturating_sub(3);
            return true;
        }
        // A preview selection is anchored to absolute scrollback lines, not screen cells,
        // so scrolling does not invalidate it and it is deliberately not cleared here.
        if let Some(ref mut diff) = self.diff_view {
            diff.scroll_up(STEP);
            return true;
        }
        // Live-send lets the user scroll the preview to read agent history without
        // exiting, but list scroll is suppressed: changing the selection mid-live-send
        // would silently aim the next keystroke at another pane. Other modals swallow
        // scroll entirely.
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
        // Full-screen app: send the wheel to it rather than scrolling the irrelevant
        // normal-buffer capture. Fires in live-send and passive preview.
        if self.forward_wheel_to_preview(true, col, row) {
            self.preview_scroll_offset = 0;
            return true;
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

    /// Map a `(col, row)` inside the list's inner rect to a `flat_items` index, or `None`
    /// for rows that resolve to no item (search bar, `[N more]` indicators, empty list,
    /// outside the rect, dialog open, diff view). Shared by `handle_click` and
    /// `hovered_index` so selection and hover use the same math.
    ///
    /// Live-send is deliberately not treated as a blocking dialog: `has_dialog()` is true
    /// during live mode so other surfaces stay frozen, but clicking a list row is how the
    /// user switches the live target.
    pub(super) fn resolve_row_to_index(&self, col: u16, row: u16) -> Option<usize> {
        if self.diff_view.is_some() || self.has_non_live_send_overlay() {
            return None;
        }
        if self.flat_items.is_empty() {
            return None;
        }
        // The list and the pinned shelf render in two rects with their own scroll
        // windows, so hit-testing mirrors the render split: the shelf holds the
        // `flat_items` suffix `[list_len..]`, the list `[..list_len]`. Both recompute the
        // renderer's scroll math, so a click resolves to the row under the pointer.
        let list_len = self.shelf_start().unwrap_or(self.flat_items.len());

        let shelf = self.shelf_inner_area;
        if shelf.height > 0 && shelf.contains(Position::from((col, row))) {
            let shelf_len = self.flat_items.len() - list_len;
            let shelf_visible = shelf.height as usize;
            let shelf_cursor = self
                .cursor
                .saturating_sub(list_len)
                .min(shelf_len.saturating_sub(1));
            let scroll = crate::tui::components::scroll::calculate_scroll(
                shelf_len,
                shelf_cursor,
                shelf_visible,
            );
            let row_in = row.saturating_sub(shelf.y) as usize;
            let row_offset = if scroll.has_more_above { 1 } else { 0 };
            if row_in < row_offset {
                return None;
            }
            let item_row = row_in - row_offset;
            if item_row >= scroll.list_visible {
                return None;
            }
            let idx = list_len + scroll.scroll_offset + item_row;
            return (idx < self.flat_items.len()).then_some(idx);
        }

        let inner = self.list_inner_area;
        if !inner.contains(Position::from((col, row))) {
            return None;
        }
        let visible_height = if self.search_bar_visible() {
            (inner.height as usize).saturating_sub(1)
        } else {
            inner.height as usize
        };
        if visible_height == 0 {
            return None;
        }
        let row_in_inner = row.saturating_sub(inner.y) as usize;
        if self.search_bar_visible() && row_in_inner + 1 == inner.height as usize {
            return None;
        }

        // Cursor may sit in the shelf; clamp it into the list range so the
        // list's scroll offset matches what the renderer computed.
        let list_cursor = self.cursor.min(list_len.saturating_sub(1));
        let scroll =
            crate::tui::components::scroll::calculate_scroll(list_len, list_cursor, visible_height);
        let row_offset = if scroll.has_more_above { 1 } else { 0 };
        if row_in_inner < row_offset {
            return None;
        }
        let item_row = row_in_inner - row_offset;
        if item_row >= scroll.list_visible {
            return None;
        }
        let abs_idx = scroll.scroll_offset + item_row;
        (abs_idx < list_len).then_some(abs_idx)
    }

    /// The hovered `flat_items` index from the last mouse position, `None` off the list or
    /// over a row that resolves to no item. Recomputed per call so a wheel scroll moves
    /// the hover with the items under the cursor.
    pub(super) fn hovered_index(&self) -> Option<usize> {
        self.mouse_pos
            .and_then(|(c, r)| self.resolve_row_to_index(c, r))
    }

    /// Handle a right-click at `(col, row)`: on a sidebar row, move the cursor there (so
    /// Rename/Delete target what was clicked) and open the context menu anchored to the
    /// click, which the renderer clamps into view.
    ///
    /// True when a menu opened. A real row opens the per-row Rename/Delete menu, empty
    /// space inside the list opens the empty-sidebar menu, and anywhere else is a no-op so
    /// the caller falls through.
    pub fn handle_right_click(&mut self, col: u16, row: u16) -> bool {
        // `resolve_row_to_index` already short-circuits while any non-live-send overlay is
        // open and inside the diff takeover, so no extra gating here.
        let anchor = (col.saturating_add(1), row.saturating_add(1));
        if let Some(idx) = self.resolve_row_to_index(col, row) {
            if self.cursor != idx {
                self.cursor = idx;
                self.update_selected();
            }
            // Mirror the web sidebar's row-aware copy so a group row reads as "Rename
            // Group / Delete Group".
            // The synthetic Trash / Archived headers are `Item::Group` rows but not user
            // groups, so route them to the bulk menus (Empty Trash / Restore All /
            // collapse), keyed off the sentinel path and the section's collapsed state.
            // Only the exact top-level headers qualify; nested project sub-folders keep
            // normal group handling, since the bulk actions would act on the whole
            // section.
            if let super::Item::Group {
                path, collapsed, ..
            } = &self.flat_items[idx]
            {
                if crate::session::is_trash_section_path(path) {
                    self.context_menu =
                        Some(ContextMenuDialog::for_trash_section(anchor, *collapsed));
                    return true;
                }
                if crate::session::is_archived_section_path(path) {
                    self.context_menu =
                        Some(ContextMenuDialog::for_archived_section(anchor, *collapsed));
                    return true;
                }
            }
            let is_group = matches!(self.flat_items[idx], super::Item::Group { .. });
            // A real project header in project view gets the pin menu; the cursor was
            // just moved onto this row, so `project_group_at_cursor` reflects it.
            // Manual and synthetic group rows keep Rename/Delete.
            let project_label = self.project_group_at_cursor();
            self.context_menu = Some(if let Some(label) = project_label {
                ContextMenuDialog::for_project_group(anchor, self.is_project_label_pinned(&label))
            } else if is_group {
                ContextMenuDialog::for_group(anchor)
            } else {
                let (is_archived, is_snoozed, is_unread) = match &self.flat_items[idx] {
                    super::Item::Session { id, .. } => self
                        .get_instance(id)
                        .map(|inst| (inst.is_archived(), inst.is_snoozed(), inst.is_unread()))
                        .unwrap_or((false, false, false)),
                    super::Item::Group { .. } => (false, false, false),
                };
                // Snooze is an Attention-sort triage primitive: the `'h'`
                // keybinding only fires in Attention sort, so the menu omits
                // the Snooze row everywhere else to keep the mouse and keyboard
                // paths in step.
                let snooze = (self.sort_order == crate::session::config::SortOrder::Attention)
                    .then_some(is_snoozed);
                // The unread toggle is always-on (any sort), so it shows
                // whenever the feature is enabled.
                let unread = crate::session::unread_enabled().then_some(is_unread);
                // Show "Fork session" only when the agent can actually fork, so
                // a resume-only agent doesn't offer an action the palette would
                // refuse. Matches the web sidebar's `acp_can_fork` gating.
                let can_fork = match &self.flat_items[idx] {
                    super::Item::Session { id, .. } => self.session_can_fork(id),
                    super::Item::Group { .. } => false,
                };
                // View switching mirrors the web sidebar's per-session
                // switch action: offered when a structured session can go
                // back to a terminal, or a terminal session's tool is
                // ACP-capable. The swap runs through the daemon.
                let switch_view = match &self.flat_items[idx] {
                    super::Item::Session { id, .. } => self.session_switch_view_target(id),
                    super::Item::Group { .. } => None,
                };
                ContextMenuDialog::for_session(
                    anchor,
                    is_archived,
                    snooze,
                    unread,
                    can_fork,
                    switch_view,
                )
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
                // Same cursor-on-the-clicked-row guarantee as ToggleArchive: snoozing an
                // active row opens the duration picker, unsnoozing wakes it.
                if let Err(e) = self.toggle_snooze_at_cursor() {
                    tracing::error!("toggle_snooze_at_cursor (context menu) failed: {}", e);
                }
            }
            ContextMenuAction::ToggleUnread => {
                // Same cursor-on-the-clicked-row guarantee as ToggleArchive.
                if let Err(e) = self.toggle_unread_at_cursor() {
                    tracing::error!("toggle_unread_at_cursor (context menu) failed: {}", e);
                }
            }
            ContextMenuAction::NewSession => self.open_new_session_dialog(),
            // The right-click already moved the cursor onto the row, so reuse the "new
            // from selection" path: a session row prefills its own repo path and group, a
            // group row borrows a member's, as `'N'` does.
            ContextMenuAction::NewFromSelection => self.open_new_from_selection(),
            ContextMenuAction::Fork => self.open_fork_from_selection(),
            ContextMenuAction::SwitchView => self.prompt_switch_view_for_selected(),
            ContextMenuAction::OpenSortPicker => self.show_sort_picker(),
            ContextMenuAction::AddProject => self.open_add_project_for_selected(),
            ContextMenuAction::OpenGroupPicker => self.show_group_picker(),
            ContextMenuAction::TogglePin => {
                // The right-click already moved the cursor onto the project header, so
                // the toggle acts on the project the menu was opened for.
                self.toggle_project_pin_at_cursor();
            }
            // The section-header actions read which synthetic section the cursor is on.
            // Empty Trash routes through a confirm; the rest act immediately.
            ContextMenuAction::EmptyTrash => self.prompt_empty_trash(),
            ContextMenuAction::RestoreAll => match self.section_at_cursor() {
                Some(SidebarSection::Trash) => self.restore_all_from_trash(),
                Some(SidebarSection::Archived) => self.unarchive_all(),
                None => {}
            },
            ContextMenuAction::ToggleSectionCollapse => match self.section_at_cursor() {
                Some(SidebarSection::Trash) => self.toggle_trashed_section(),
                Some(SidebarSection::Archived) => self.toggle_archived_section(),
                None => {}
            },
        }
    }

    /// Which synthetic section the cursor's `Item::Group` header belongs to, for the
    /// section context-menu actions the right-click has already parked the cursor on.
    pub(super) fn section_at_cursor(&self) -> Option<SidebarSection> {
        match self.flat_items.get(self.cursor) {
            Some(super::Item::Group { path, .. }) => {
                if crate::session::is_trash_section_path(path) {
                    Some(SidebarSection::Trash)
                } else if crate::session::is_archived_section_path(path) {
                    Some(SidebarSection::Archived)
                } else {
                    None
                }
            }
            _ => None,
        }
    }

    /// Open the new-session dialog with the same gating `'n'` applies: a "please wait"
    /// info dialog while a session is being created, the no-agents dialog when no tool is
    /// available, otherwise the full dialog. Shared by `'n'` and the empty-sidebar click.
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
        // Opening `n` with a row selected is exactly where `N` (new-from-selection) would
        // have helped, so count it; past the threshold the tip earns its way into the
        // badge and a one-time pop (#2262).
        if self.selected_session.is_some() || self.selected_group.is_some() {
            self.record_new_session_with_selection();
        }
        let existing_groups: Vec<String> =
            self.all_groups().iter().map(|g| g.path.clone()).collect();
        let current_profile = self.config_profile();
        let profiles =
            list_profiles_for_display().unwrap_or_else(|_| vec![current_profile.clone()]);
        self.new_dialog = Some(NewSessionDialog::new(
            self.available_tools.clone(),
            existing_groups,
            &current_profile,
            profiles,
        ));
    }

    /// Open the tips overlay from `crate::tips`, shared by the palette, the `?` screen
    /// and the badge. Always opens, even with nothing eligible, so an explicit "Show tips"
    /// gives an empty state rather than silence.
    pub(super) fn open_tips_dialog(&mut self) {
        let config = load_config().ok().flatten().unwrap_or_default();
        let signals = crate::tips::TipSignals {
            new_session_with_selection_count: config.app_state.new_session_with_selection_count,
            used_new_from_selection: config.app_state.used_new_from_selection,
            system_health_tip_earned: config.app_state.system_health_tip_earned,
            used_system_health: config.app_state.used_system_health,
        };
        let eligible = crate::tips::eligible(crate::tips::TipSurface::Tui, &signals);
        self.tips_dialog = Some(TipsDialog::new(
            eligible,
            config.app_state.tips_seen.clone(),
            !config.session.show_tips,
            self.strict_hotkeys,
        ));
    }

    /// Persist what the tips overlay reported on close: merge newly-seen ids into
    /// `tips_seen` and apply a "don't show tips" toggle. Merging rather than overwriting
    /// preserves seen ids for tips that aren't currently eligible.
    pub(super) fn persist_tips_outcome(&mut self, outcome: TipsOutcome) {
        if outcome.newly_seen.is_empty() && outcome.disabled.is_none() {
            return;
        }
        let newly_seen = outcome.newly_seen;
        if let Err(e) = update_app_state(|state| {
            for id in newly_seen {
                if !state.tips_seen.iter().any(|s| s == &id) {
                    state.tips_seen.push(id);
                }
            }
        }) {
            tracing::warn!(target: "tui.input", "Failed to persist tips state: {}", e);
        }
        if let Some(disabled) = outcome.disabled {
            if let Err(e) = update_config(|config| {
                config.session.show_tips = !disabled;
            }) {
                tracing::warn!(target: "tui.input", "Failed to persist tips state: {}", e);
            }
        }
        if let Ok(config) = load_config().map(|c| c.unwrap_or_default()) {
            self.tips_unseen = super::tips_unseen_count(&config);
        }
    }

    /// Bump the "opened new-session with a selection" counter that earns the
    /// new-from-selection tip (#2262), persist it, and refresh the badge.
    fn record_new_session_with_selection(&mut self) {
        if let Err(e) = update_app_state(|state| {
            state.new_session_with_selection_count =
                state.new_session_with_selection_count.saturating_add(1);
        }) {
            tracing::warn!(target: "tui.input", "Failed to persist tip signal: {}", e);
            return;
        }
        if let Ok(config) = load_config().map(|c| c.unwrap_or_default()) {
            self.tips_unseen = super::tips_unseen_count(&config);
        }
    }

    /// Record that the user has used `N`, so the tip teaching it is suppressed, a queued
    /// pop is cancelled and the badge refreshes. Idempotent.
    fn record_used_new_from_selection(&mut self) {
        // They know about N now; don't pop the tip that teaches it.
        if self.pending_tip_pop.map(|t| t.id) == Some("new-from-selection") {
            self.pending_tip_pop = None;
        }
        let already_used = load_config()
            .ok()
            .flatten()
            .is_some_and(|c| c.app_state.used_new_from_selection);
        if already_used {
            return;
        }
        if let Err(e) = update_app_state(|state| {
            state.used_new_from_selection = true;
        }) {
            tracing::warn!(target: "tui.input", "Failed to persist tip signal: {}", e);
            return;
        }
        if let Ok(config) = load_config().map(|c| c.unwrap_or_default()) {
            self.tips_unseen = super::tips_unseen_count(&config);
        }
    }

    /// After the new-session dialog closes, queue an earned tip if one just became
    /// eligible and tips aren't disabled. `drain_pending_tip_pop` shows it on the next
    /// keystroke, so it never interrupts an in-flight action.
    pub(super) fn queue_earned_tip_pop(&mut self) {
        if self.pending_tip_pop.is_some() {
            return;
        }
        let config = load_config().ok().flatten().unwrap_or_default();
        if !config.session.show_tips {
            return;
        }
        let signals = crate::tips::TipSignals {
            new_session_with_selection_count: config.app_state.new_session_with_selection_count,
            used_new_from_selection: config.app_state.used_new_from_selection,
            system_health_tip_earned: config.app_state.system_health_tip_earned,
            used_system_health: config.app_state.used_system_health,
        };
        self.pending_tip_pop = crate::tips::next_earned_pop(
            crate::tips::TipSurface::Tui,
            &config.app_state.tips_seen,
            &signals,
        );
    }

    /// Open the queued earned tip as a one-tip overlay, if any. Called when the home view
    /// is idle so the pop never interrupts an action. True when one opened, so the caller
    /// can treat the triggering keystroke as consumed.
    pub(super) fn drain_pending_tip_pop(&mut self) -> bool {
        let Some(tip) = self.pending_tip_pop.take() else {
            return false;
        };
        let config = load_config().ok().flatten().unwrap_or_default();
        // Re-check: the user may have disabled tips between queueing and now.
        if !config.session.show_tips {
            return false;
        }
        self.tips_dialog = Some(TipsDialog::new(
            vec![tip],
            config.app_state.tips_seen.clone(),
            !config.session.show_tips,
            self.strict_hotkeys,
        ));
        true
    }

    /// Left-click on the empty area of the sidebar: a quick "drop out of live mode"
    /// gesture, and otherwise a no-op. The "open new session" entry moved to the
    /// right-click menu so left-clicking empty space stays low-stakes.
    ///
    /// Fires only when no overlay is up and the diff view is closed, so a click on an
    /// empty list under a modal doesn't punch through.
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

    /// Open the rename dialog for the sidebar's selection (a session row, or a
    /// manual-mode group). Project and organization groups are derived, so they raise an
    /// info dialog explaining how to switch modes. No-op with nothing selected, or when
    /// the session is mid-create or mid-delete, where a rename would race the cascade.
    ///
    /// Shared by the `'r'` / `'R'` handlers and the context menu.
    pub(super) fn open_rename_for_selected(&mut self) {
        if let Some(id) = self.selected_session.clone() {
            let Some(inst) = self.get_instance(&id) else {
                return;
            };
            if matches!(inst.status, Status::Deleting | Status::Creating) {
                return;
            }
            // Rename is anchored to the selected session, so the dialog opens against
            // that session's profile, not the view-level one, which differs in
            // all-profiles mode.
            let current_profile = inst.source_profile.clone();
            let title = inst.title.clone();
            let group_path = inst.group_path.clone();
            // Capture branch context up front; a tied aoe-managed worktree
            // can opt to rename the branch alongside the directory.
            let branch_ctx = inst
                .worktree_info
                .as_ref()
                .map(|w| (w.branch.clone(), w.main_repo_path.clone()));

            let profiles =
                list_profiles_for_display().unwrap_or_else(|_| vec![current_profile.clone()]);
            let existing_groups: Vec<String> =
                self.all_groups().iter().map(|g| g.path.clone()).collect();
            let mut dialog = RenameDialog::new(
                &title,
                &group_path,
                &current_profile,
                profiles,
                existing_groups,
            );
            if self.tie_workdir_applies_for(&id) {
                if let Some((branch, main_repo)) = branch_ctx {
                    // The upstream probe is a quick `git for-each-ref`; this
                    // is a one-shot on dialog open, not a hot path.
                    let upstream =
                        crate::git::GitWorktree::new(std::path::PathBuf::from(&main_repo))
                            .ok()
                            .and_then(|g| g.branch_upstream(&branch));
                    dialog = dialog.with_worktree_branch(&branch, upstream);
                }
            }
            self.rename_dialog = Some(dialog);
        } else if let Some(group_path) = &self.selected_group {
            if let Some((title, hint)) = self.automatic_group_hint() {
                self.info_dialog = Some(InfoDialog::new(&title, &hint));
                return;
            }
            let group_path = group_path.clone();
            let current_profile = self
                .selected_group_profile
                .clone()
                .unwrap_or_else(|| self.config_profile());
            let profiles =
                list_profiles_for_display().unwrap_or_else(|_| vec![current_profile.clone()]);
            // Duplicate-name validation is per-profile (rename_selected_group checks only
            // the target profile's tree), so the dialog's existing names must be scoped to
            // this group's profile; spanning all profiles would falsely block a rename
            // that only collides in another profile.
            let existing_groups: Vec<String> = self
                .group_trees
                .get(&current_profile)
                .map(|t| t.get_all_groups().iter().map(|g| g.path.clone()).collect())
                .unwrap_or_default();
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

    /// Open the edit-workdir-name dialog for the selected session, valid only for an
    /// aoe-managed worktree session that is not running; other cases get an info dialog.
    pub(super) fn open_worktree_name_for_selected(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        // Tied mode (#1927) collapses naming into one Rename action, since the directory
        // follows the title, so route the standalone workdir edit to the rename dialog.
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

    /// Open the delete dialog (or a force-remove confirm, or the group delete-options
    /// dialog) for the current selection, mirroring the `'d'` / `'D'` gating: Terminal
    /// view rejects deletion with an info dialog, Creating sessions are inert,
    /// stuck-Deleting sessions get a force-remove confirm, and Project and organization
    /// groups can't be deleted. Shared by the keys and the context menu.
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
                         (the sandbox container is torn down; worktrees and branches will not be cleaned up)",
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

                // Trash-first: with session.delete_to_trash on (the default) an untrashed
                // session moves to the trash instead of opening the permanent-delete
                // dialog. A row already in Trash falls through, so `D` there deletes
                // permanently. See #2489.
                let already_trashed = inst.is_trashed();
                // Resolve the policy from the selected session's profile, not the active
                // filter: in all-profiles view `config_profile()` can differ from the row's
                // `source_profile` and would apply the wrong retention policy. See #2489.
                let session_cfg =
                    crate::session::resolve_config_or_warn(&inst.source_profile).session;
                let delete_to_trash = session_cfg.delete_to_trash;
                if delete_to_trash && !already_trashed {
                    let sid = session_id.clone();
                    // With session.confirm_delete on (the default), guard the trash with
                    // a confirmation instead of trashing on the keystroke. The delete key
                    // accepts the dialog, so the deliberate gesture is two taps of one key
                    // while a stray keystroke is harmless; the accept path runs the same
                    // trash_session_by_id.
                    if session_cfg.confirm_delete {
                        // Read the accept key off the binding table so relocating Delete
                        // can't drift the hint from the key that opened the dialog. A
                        // chord that isn't a bare character can't be a confirm char, so it
                        // falls back to the dialog's own y/Enter.
                        let delete_key = bindings::label(ActionId::Delete, self.strict_hotkeys);
                        let mut key_chars = delete_key.chars();
                        let accept_char = match (key_chars.next(), key_chars.next()) {
                            (Some(c), None) => Some(c),
                            _ => None,
                        };
                        let hint = match accept_char {
                            Some(_) => {
                                format!("Press {delete_key} again to confirm, Esc to cancel.")
                            }
                            None => "Press y to confirm, Esc to cancel.".to_string(),
                        };
                        let message = format!("Move '{}' to the trash?\n{hint}", inst.title);
                        self.pending_trash_session = Some(sid);
                        // Offer the same in-dialog opt-out the quit confirm has: the
                        // guard is on by default, so a user who wants one-keystroke trash
                        // back shouldn't have to find the setting. Ticking it persists
                        // confirm_delete = false.
                        let mut dialog =
                            ConfirmDialog::new("Confirm Delete", &message, "trash_session")
                                .buttons("Delete", "Cancel")
                                .offering_dont_ask_again();
                        if let Some(c) = accept_char {
                            dialog = dialog.confirmed_by(c);
                        }
                        self.confirm_dialog = Some(dialog);
                        return;
                    }
                    self.trash_session_by_id(&sid);
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
            if let Some((title, hint)) = self.automatic_group_hint() {
                self.info_dialog = Some(InfoDialog::new(&title, &hint));
                return;
            }
            // Scope the count to the selected group's profile: two profiles can share a
            // group path, and counting by path alone would pop the "delete N sessions"
            // dialog for an empty group whose same-named twin still has rows.
            let owning_profile = self.selected_group_profile.clone();
            let prefix = format!("{}/", group_path);
            let session_count = self
                .instances
                .values()
                .filter(|i| {
                    (i.group_path == *group_path || i.group_path.starts_with(&prefix))
                        && owning_profile
                            .as_ref()
                            .is_none_or(|p| &i.source_profile == p)
                })
                .count();

            if session_count > 0 {
                let has_managed_worktrees = self.group_has_managed_worktrees(
                    group_path,
                    &prefix,
                    owning_profile.as_deref(),
                );
                let has_containers =
                    self.group_has_containers(group_path, &prefix, owning_profile.as_deref());
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

    /// Route a left-click inside the session list. A single click on a session row
    /// selects it and requests live-send for that row (the same `Action::EnterLiveSend`
    /// Tab emits); a single click on a group row toggles its collapse; a second click on
    /// the same session row within `DOUBLE_CLICK_THRESHOLD` activates it, as `Enter`
    /// would, so a full tmux attach stays reachable. Returns the action to dispatch, or
    /// `None` for no-op clicks. The caller redraws unconditionally so the moved cursor
    /// paints before the action runs. Gated by `has_dialog()` through
    /// `resolve_row_to_index`, so clicks don't shift selection under an open modal.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<Action> {
        self.handle_click_at(std::time::Instant::now(), col, row)
    }

    /// Same as `handle_click` with a caller-supplied `now`, so unit tests can drive
    /// double-click detection without sleeping.
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
            // The first click already selected the row and toggled a group, so the second
            // only activates a session; re-toggling would undo the first and flicker.
            //
            // `cursor` is re-synced to `abs_idx` before activating because anything
            // between the clicks (an arrow key, a poll-driven re-sort) can move it, and
            // `activate_selected_session()` reads `selected_session`, which tracks the
            // cursor rather than the click target.
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
                self.system_health_open = false;
                if self.cursor != abs_idx {
                    self.cursor = abs_idx;
                    self.update_selected();
                }
                // An archived row is parked, its pane killed on archive. A single click
                // is a "let me look at this" gesture, so it must not enter live-send,
                // which would respawn the pane and auto-unarchive the row through
                // touch_last_accessed. Stop at the cursor update; bringing it back stays
                // explicit (`z`, or a deliberate double-click / Enter).
                let archived = self
                    .get_instance(&id)
                    .map(|inst| inst.is_archived())
                    .unwrap_or(false);
                // Single-click behavior is otherwise `SessionConfig::click_action`:
                // `LiveSend` (the default) enters live-send for the clicked row or
                // switches the live target, while `SelectOnly` stops at the cursor update
                // so the user can browse previews, exiting live mode if a different row
                // was live. Double-click still activates via `default_attach_mode`.
                // `click_action` returns `None` for structured sessions, where
                // `start_live_send` already short-circuits.
                if archived
                    || matches!(
                        self.click_action(&id),
                        Some(crate::session::ClickAction::SelectOnly)
                    )
                {
                    // The click only moves the cursor and, if live-sending, leaves live
                    // mode. That holds for a different row (keystrokes were aimed at the
                    // old session) and for the row already live: a single click is a "stop
                    // touching that" gesture. In `LiveSend` mode the `start_live_send`
                    // branch below retargets instead.
                    if let Some(state) = self.live_send.clone() {
                        self.exit_live_send_and_restore_sizing(&state);
                    }
                    None
                } else {
                    self.start_live_send()
                }
            }
        }
    }

    /// A double-click on the preview pane produces the same `Action` a sidebar
    /// double-click or `Enter` would, so the gestures match. Mirrors `handle_click`'s
    /// detection but keyed to the preview rect via `last_preview_click`, and gated to a
    /// plain left press over the preview with no overlay. The previewed pane is always
    /// `selected_session`, so it activates the right row without touching `cursor`.
    /// Returns the action on the second qualifying press on the same cell within
    /// `DOUBLE_CLICK_THRESHOLD`, else `None`, recording that press's timing. Shift falls
    /// through to aoe's own preview selection, matching the mouse-forward gate.
    pub fn preview_double_click_action(
        &mut self,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
        col: u16,
        row: u16,
    ) -> Option<Action> {
        self.preview_double_click_action_at(std::time::Instant::now(), kind, modifiers, col, row)
    }

    /// Forget the press that would have paired into a double-click, once it has been
    /// spent on something else, so clicking a link twice does not also activate the
    /// session.
    pub fn forget_preview_click(&mut self) {
        self.last_preview_click = None;
    }

    /// Record the link under `(col, row)` for the status bar. Returns whether
    /// it changed, so the caller only repaints when the answer moved.
    pub fn update_hovered_link(&mut self, col: u16, row: u16) -> bool {
        let cell = Some((col, row));
        if cell == self.hover_cell {
            return false;
        }
        // Report whether the resolved LINK moved, not the pointer: tracking a
        // pointer across one link must not repaint on every motion event.
        let before = self.hovered_link();
        self.hover_cell = cell;
        before != self.hovered_link()
    }

    /// The link the pointer is resting on, if any.
    pub(in crate::tui) fn hovered_link(&self) -> Option<String> {
        let (col, row) = self.hover_cell?;
        self.preview_link_at(col, row)
    }

    /// The link under `(col, row)`, if one is painted there this frame. The preview's
    /// transport strips OSC 8 before ratatui sees it, so the target is recovered by
    /// matching the pane's advertised link text against the row, or by finding a plain URL
    /// in the row (`crate::tui::links`).
    pub fn preview_link_at(&self, col: u16, row: u16) -> Option<String> {
        if self.has_non_live_send_overlay() {
            return None;
        }
        // A mounted structured transcript owns the pane and supplies its own rows, but
        // `active_preview_cache` still returns the tmux capture and `preview_text_view`
        // came from the transcript's geometry. Resolving one against the other opens a URL
        // from another session's output, with no underline to warn the user. Same
        // line-source branch `extract_preview_selection_text` makes.
        let structured_owns_pane = self
            .structured_preview
            .as_ref()
            .zip(self.selected_session.as_deref())
            .is_some_and(|(view, id)| view.session_id() == id);
        if structured_owns_pane {
            return None;
        }
        let view = self.preview_text_view;
        if !view.contains(col, row) {
            return None;
        }
        let cache = self.active_preview_cache();
        let line = cache
            .parsed_text
            .as_ref()?
            .lines
            .get(view.abs_line_at_row(row))?;
        let offset = col - view.pane.x;
        crate::tui::links::link_spans_for_line(line, view.pane.width, &cache.links)
            .into_iter()
            .find(|span| (span.start..span.end).contains(&offset))
            .map(|span| span.uri)
    }

    /// Same as `preview_double_click_action`, but the caller supplies `now` so
    /// unit tests can drive double-click detection deterministically.
    pub(super) fn preview_double_click_action_at(
        &mut self,
        now: std::time::Instant,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
        col: u16,
        row: u16,
    ) -> Option<Action> {
        use crossterm::event::{MouseButton, MouseEventKind};
        if !matches!(kind, MouseEventKind::Down(MouseButton::Left)) {
            return None;
        }
        if modifiers.contains(crossterm::event::KeyModifiers::SHIFT) {
            return None;
        }
        if self.has_non_live_send_overlay() || !self.hit_preview(col, row) {
            return None;
        }
        // Match the same cell, not just the row: a sidebar row identifies an item, but a
        // preview row is a line of text, so two presses on different columns of one line
        // must not count as a double-click.
        let is_double = matches!(
            self.last_preview_click,
            Some((prev_time, prev_col, prev_row))
                if prev_col == col
                    && prev_row == row
                    && now.duration_since(prev_time) <= DOUBLE_CLICK_THRESHOLD
        );
        if is_double {
            // Reset so a triple-click doesn't immediately re-fire activation.
            self.last_preview_click = None;
            self.activate_selected_session()
        } else {
            self.last_preview_click = Some((now, col, row));
            None
        }
    }

    /// Record the mouse position from a `Moved` event so the list can hover-highlight the
    /// row under the cursor; `mouse_pos` clears when the cursor leaves `list_inner_area`.
    /// True only when the resolved hovered item changes, so the caller can skip a redraw
    /// on every mouse twitch.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        // Open overlays get hover routed first so their focus highlight tracks the mouse.
        // The sidebar's own hover state still updates underneath, so the row highlight is
        // right the instant the dialog closes.
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
        if let Some(dialog) = &mut self.attach_project_dialog {
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

        // Footer-toolbar hover: the hovered button's shortcut drives the inverted-chip
        // highlight on the next render, recomputed against the current button rects so it
        // clears as the pointer leaves.
        let prev_footer_hover = self.footer_hover;
        self.footer_hover = self
            .footer_buttons
            .iter()
            .find(|(rect, _)| rect.contains(Position::from((col, row))))
            .map(|(_, key)| *key);
        let footer_changed = prev_footer_hover != self.footer_hover;

        let diagnostics_hovered = !self.has_non_live_send_overlay()
            && self.diagnostics_area.contains(Position::from((col, row)));
        let diagnostics_changed = diagnostics_hovered != self.diagnostics_hovered;
        self.diagnostics_hovered = diagnostics_hovered;

        // Hover is live over both the scrolling list and the pinned shelf, so a shelf row
        // highlights like a list row; `resolve_row_to_index` maps either region.
        let over_sidebar = self.list_inner_area.contains(Position::from((col, row)))
            || self.shelf_inner_area.contains(Position::from((col, row)));
        let new_pos = if over_sidebar { Some((col, row)) } else { None };
        let prev_idx = self.hovered_index();
        self.mouse_pos = new_pos;
        let new_idx = self.hovered_index();

        // Footer tips badge: highlight on hover like a session row, gated to no overlay
        // being open (the badge isn't clickable then), matching `handle_tips_badge_click`.
        let badge_hover = !self.has_non_live_send_overlay()
            && self
                .tips_badge_rect
                .is_some_and(|r| r.contains(Position::from((col, row))));
        let badge_changed = badge_hover != self.tips_badge_hovered;
        self.tips_badge_hovered = badge_hover;

        overlay_changed
            || footer_changed
            || diagnostics_changed
            || badge_changed
            || prev_idx != new_idx
    }

    /// Route a mouse-wheel-down at (col, row); see handle_scroll_up.
    pub fn handle_scroll_down(&mut self, col: u16, row: u16) -> bool {
        const STEP: u16 = 3;
        // Settings takeover owns the wheel; see handle_scroll_up.
        if let Some(view) = &mut self.settings_view {
            return view.handle_wheel_scroll(false);
        }
        if self.system_health_open && self.hit_preview(col, row) {
            let visible_rows = crate::tui::components::diagnostics::agent_table_visible_rows(
                self.preview_area.height,
            );
            let max = self.metrics.agents.len().saturating_sub(visible_rows);
            self.system_health_scroll = self.system_health_scroll.saturating_add(3).min(max);
            return true;
        }
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
        // Mirror handle_scroll_up: a full-screen app gets the wheel
        // forwarded rather than moving the preview's capture window.
        if self.forward_wheel_to_preview(false, col, row) {
            self.preview_scroll_offset = 0;
            return true;
        }
        if self.preview_scroll_offset == 0 {
            return false;
        }
        self.preview_scroll_offset = self.preview_scroll_offset.saturating_sub(STEP);
        true
    }

    /// Route a bracketed paste to the active text input.
    ///
    /// Live-send wins over every dialog as long as nothing is stacked on top of it: a
    /// paste while "attached" streams to the agent's pane. Once an overlay is open over
    /// live-send, the paste goes to that overlay, exactly like the key path, or the user
    /// would watch their clipboard land in the pane behind a focused input. Text-input
    /// dialogs come next so multi-line dictation lands where the user is typing. The
    /// settings view is last: its paste handler strips newlines.
    pub fn handle_paste(&mut self, text: &str) {
        // Any paste drops a finalized preview selection, like `handle_key` does up front:
        // the highlight pins to cell coords, so once the user pastes anywhere the cells
        // underneath can change. Doing it here covers the drag-select -> right-click ->
        // paste-into-dialog sequence, which never goes through `handle_key`.
        self.clear_preview_selection();
        if !self.has_non_live_send_overlay() {
            if let Some(state) = self.live_send.clone() {
                if let Some(worker) = &self.live_send_worker {
                    for key in split_paste_for_live_send(text) {
                        worker.send(key);
                    }
                }
                self.stamp_last_accessed(&state.session_id);
                return;
            }
        }
        // Before send_message_dialog below: with the skills manager open and its editor
        // focused, a paste falling through would open a message dialog aimed at whatever
        // session was selected before the panel opened, rendering both overlays.
        if let Some(ref mut dialog) = self.skills_manager_dialog {
            dialog.handle_paste(text);
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

        // No dialog open: route the paste into a new compose dialog when the selected
        // session is runnable, else stash it in pending_paste for the next dialog open.
        // Losing dictation is worse than silently catching it.
        if let Some((id, title, target)) = self.resolve_send_target() {
            let label = live_send::format_target_label(&title, &target);
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

    /// Open the restart dialog for the selected session, pre-filled with its profile and
    /// AI engine; submit restarts it, optionally migrating profile and/or swapping the
    /// engine. No-op with no selection or a mid-transition session.
    fn open_restart_dialog(&mut self) {
        // Match the new-session paths: bail with the no-agents modal when no tool is
        // installed, rather than opening a picker with an empty tool list.
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
        let profiles =
            list_profiles_for_display().unwrap_or_else(|_| vec![current_profile.clone()]);
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

    /// Try to enter live-send against the selected session. Unlike `resolve_send_target`
    /// this does not require the pane to exist: `prepare_live_send` calls
    /// `ensure_pane_ready`, which revives stopped sessions, so Tab does not silently no-op
    /// on dead-but-recoverable rows.
    ///
    /// Still a no-op on group headers, empty lists and Creating rows, and on a session
    /// that is already the live-send target, so clicking the same row twice does not
    /// re-run ensure_pane_ready or drop the live worker.
    pub(super) fn start_live_send(&mut self) -> Option<Action> {
        let id = self.selected_session.clone()?;
        if self.live_send.as_ref().is_some_and(|s| s.session_id == id) {
            return None;
        }
        let inst = self.get_instance(&id)?;
        if matches!(inst.status, Status::Creating | Status::Deleting) {
            return None;
        }
        // Acp-mode sessions are not tmux-backed, so live-send has no target: no-op rather
        // than enqueue an `Action::EnterLiveSend` that would fail downstream.
        if inst.is_structured() {
            return None;
        }
        // Pick the target from the pane being previewed: Structured view to the agent
        // pane, Terminal view to the paired host or container terminal so 'm'/Tab compose
        // against the shell on screen, Tool view to the named tool's pane.
        self.pending_live_send_target = match &self.view_mode {
            ViewMode::Structured => live_send::LiveSendTarget::Agent,
            ViewMode::Terminal => {
                if inst.is_sandboxed() && self.get_terminal_mode(&id) == TerminalMode::Container {
                    live_send::LiveSendTarget::ContainerTerminal
                } else {
                    live_send::LiveSendTarget::Terminal
                }
            }
            ViewMode::Tool(name) => live_send::LiveSendTarget::Tool(name.clone()),
        };
        Some(Action::EnterLiveSend(id))
    }

    /// Auto-start live-send after an explicit view switch (`ToggleView`,
    /// opening a tool session) when the `Auto Live-Send On View Switch`
    /// setting is on for the selected session's resolved config. `None`
    /// when there's no selected session or the setting is off; the
    /// caller then leaves the plain view switch alone. Deliberately not
    /// wired into list navigation/selection: this only fires from the
    /// explicit view-switch call sites that invoke it.
    pub(super) fn maybe_auto_start_live_send(&mut self) -> Option<Action> {
        let id = self.selected_session.clone()?;
        if !self.live_send_on_view_switch(&id) {
            return None;
        }
        self.start_live_send()
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
            // Command letters match only when unmodified: the leader-again passthrough
            // already claimed the modified form, and folding `Ctrl+K` / `Alt+b` into a
            // command would surprise users reaching for a chord. Shift is allowed, since
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
                // Esc, or any unbound or modified key, cancels the menu without
                // forwarding: the leader already swallowed the keystroke, as tmux's
                // prefix does for unknown keys.
                _ => {}
            }
            return;
        }

        // `handle_key` already cleared any finalized preview selection at the top, which
        // covers the PageUp/PageDown scroll keys below too.

        // Shift+PageUp / Shift+PageDown scroll the preview without forwarding, matching
        // the terminal-emulator convention where shift+page works on the outer scrollback.
        // Bare PageUp/PageDown still goes to the agent so agents that page their own UI
        // keep working.
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

        // The exit chord is checked before drift: exiting is always safe, and a user
        // escaping a stuck live mode shouldn't hit a "session ended" dialog on the way.
        if live_send::chord_list_matches(&state.exit_chords, key) {
            self.exit_live_send_and_restore_sizing(&state);
            return;
        }
        // Leader press: arm the live-send command menu and swallow the keystroke; the
        // next key goes to the pending-leader branch at the top. Checked after the exit
        // chord so a leader misconfigured as the exit chord still exits.
        if let Some(leader) = state.leader {
            if live_send::chord_matches(leader, key) {
                self.live_send_pending_leader = true;
                return;
            }
        }
        if self.end_live_send_on_drift(&state) {
            return;
        }
        // Ctrl+C here is forwarded to the agent rather than quitting aoe (the app-level
        // handler defers to live-send via `is_live_send_capturing`). Flash the footer so
        // the user learns the keystroke landed on the agent; re-armed per press (#2894).
        let is_ctrl_c =
            matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL);
        match live_send::translate(key) {
            live_send::LiveDispatch::Ignore => {}
            live_send::LiveDispatch::Send(tmux_key) => {
                if let Some(worker) = &self.live_send_worker {
                    worker.send(tmux_key);
                }
                if is_ctrl_c {
                    self.flash_ctrl_c_hint();
                }
                self.stamp_last_accessed(&state.session_id);
            }
        }
    }

    /// Exit live-send before an activation hands the terminal to a tmux attach. With
    /// `click_action = LiveSend` the first click of a double-click already entered
    /// live-send, and the second resolves to an attach. Without this teardown the
    /// just-spawned worker keeps dispatching against a pane we are leaving, the attach
    /// inherits the preview-pinned window size, and detaching drops the user back into
    /// live mode rather than the home list (#2290). No-op when not live-sending.
    fn exit_live_send_before_attach(&mut self) {
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
        }
    }

    /// Tear down live-send state and restore the tmux window's automatic sizing:
    /// live-send's resize loop forces manual sizing, which would leave the next attach
    /// from a full-size terminal cramped at the preview dimensions. Re-setting
    /// `window-size latest` is best-effort, so a stuck pane never blocks the exit.
    fn exit_live_send_and_restore_sizing(&mut self, state: &live_send::LiveSendState) {
        let session = crate::tmux::Session::from_name(&state.tmux_name);
        session.reset_size_to_latest_client();
        self.teardown_live_send();
    }

    /// Shared live-send teardown that touches no tmux sizing. Normal exits come through
    /// `exit_live_send_and_restore_sizing`; the lost-lock exit calls this directly,
    /// because the surface that took over has already sized the window and re-asserting
    /// `window-size latest` would stomp it.
    fn teardown_live_send(&mut self) {
        let live_session_id = self.live_send.take().map(|state| state.session_id);
        self.live_send_worker = None;
        // Leave the capture worker running: the same pane is still previewed, just at the
        // idle cadence, which the render reconcile retunes.
        self.live_send_last_resize = None;
        self.live_send_resize_retry_at = None;
        // The leader menu is live-mode-only, so drop any half-entered chord. The sidebar
        // collapse is persisted home-view state, so exiting deliberately leaves it as the
        // user set it rather than force-revealing the list.
        self.live_send_pending_leader = false;
        // The Ctrl+C footer flash is live-mode-only; drop any pending window
        // so it can't linger onto the home-view footer after exit (#2894).
        self.live_send_ctrl_c_flash_until = None;
        // Live mode just owned the pane's size, so the non-live preview must re-assert its
        // geometry on the next render now that the header is visible again.
        if let Some(id) = &live_session_id {
            self.clear_preview_pane_sync(id);
        }
        self.reseat_cursor_after_rebuild();
        // A live-mode highlight pins to the live-resized pane coords, and exiting reflows
        // the preview, so drop the selection rather than let it survive into a pane it no
        // longer points at.
        self.clear_preview_selection();
    }

    /// `Some(reason)` when the live-send target drifted out from under us since entry:
    /// the instance row was deleted, the title was renamed and the tmux session with it
    /// (a retitle whose tmux rename did not land is not drift, since `resolve_name` still
    /// resolves onto the worker's pane), or the tmux session is gone while our row says
    /// otherwise. The last check reads `session_exists_from_cache`, a hashmap probe per
    /// keystroke; a `None` entry claims no drift, leaving the row and name checks as the
    /// safety net.
    ///
    /// The caller shows the message verbatim, so phrase it as a user-facing sentence.
    fn live_send_drift_reason(&self, state: &live_send::LiveSendState) -> Option<&'static str> {
        let Some(inst) = self.get_instance(&state.session_id) else {
            return Some("Session was deleted while live mode was active.");
        };
        let current_name = match &state.target {
            live_send::LiveSendTarget::Agent => {
                crate::tmux::Session::resolve_name(&inst.id, &inst.title)
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
        if current_name != state.tmux_name {
            return Some("Session was renamed while live mode was active.");
        }
        if crate::tmux::session_exists_from_cache(&state.tmux_name) == Some(false) {
            return Some("tmux pane went away while live mode was active.");
        }
        None
    }

    pub(super) fn end_live_send_on_drift(&mut self, state: &live_send::LiveSendState) -> bool {
        let Some(reason) = self.live_send_drift_reason(state) else {
            return false;
        };
        self.exit_live_send_and_restore_sizing(state);
        self.info_dialog = Some(InfoDialog::new("Live send ended", reason));
        true
    }

    /// Poll the worker's lock-loss flag, set off-thread when another surface takes the
    /// size-owner lock, and exit live mode when it trips, mirroring the web live view's
    /// demote-on-heartbeat. Called each tick so the exit does not wait for a keystroke.
    /// True when live mode was exited.
    ///
    /// Deliberately does not restore the window's sizing: the new owner already resized
    /// the window, and `window-size latest` would stomp it.
    pub(in crate::tui) fn poll_live_send_takeover(&mut self) -> bool {
        if !self
            .live_send_worker
            .as_ref()
            .is_some_and(live_send::LiveSendWorker::lock_lost)
        {
            return false;
        }
        let Some(state) = self.live_send.clone() else {
            // Worker outlived the live-send state (already torn down some
            // other way); just drop it.
            self.live_send_worker = None;
            return false;
        };
        // A dead or renamed session also fails the worker's ownership refresh, so prefer
        // the accurate drift message over blaming a takeover that never happened.
        if self.end_live_send_on_drift(&state) {
            return true;
        }
        // Name the thief where the owner id is unambiguous: the web dashboard's live
        // viewers register as `live-*` (src/server/live_ws.rs), other TUIs as `tui-*`.
        let message = match crate::tmux::Session::from_name(&state.tmux_name).size_owner() {
            Some((id, _)) if id.starts_with("live-") => {
                "The web dashboard took over this session's live view."
            }
            Some((id, _)) if id.starts_with("tui-") => {
                "Another aoe TUI took over this session's live view."
            }
            _ => "Another surface took over this session's live view.",
        };
        self.teardown_live_send();
        self.info_dialog = Some(InfoDialog::new("Live send ended", message));
        true
    }

    /// Open the shared permission-response dialog for the selected session.
    ///
    /// Terminal sessions send the agent's own quick-response keystrokes. AoE never parses
    /// pane content to detect the prompt: the premise is that the user has already seen
    /// the CLI's question on the pane they are looking at, which is why there is no
    /// `Status::Waiting` gate.
    ///
    /// Structured sessions have no pane to have looked at, so the dialog carries the tool
    /// name, target and destructive flag from the daemon's `pending_approvals`. The nonce
    /// is captured at open time, not re-read on the keypress, so a poll tick between open
    /// and submit cannot retarget the answer (a stale nonce 404s instead).
    fn open_permission_response_dialog(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(inst) = self.get_instance(&id) else {
            return;
        };
        // Archived and trashed rows keep no live worker, so a cached approval is stale
        // and its resolve can only 404.
        if matches!(inst.status, Status::Creating | Status::Deleting)
            || inst.is_archived()
            || inst.is_trashed()
        {
            return;
        }
        let title = inst.title.clone();
        let tool = inst.tool.clone();
        if inst.is_structured() {
            let Some(approval) = self
                .structured_pending_approvals
                .get(&id)
                .and_then(|approvals| approvals.first())
                .cloned()
            else {
                self.info_dialog = Some(InfoDialog::new(
                    "No Pending Approval",
                    "The daemon has no pending approval for this session.",
                ));
                return;
            };
            // A choice list is answers, not allow/deny: the generic dialog has no labels
            // to show and its Allow would answer the agent's first option. Send the user
            // where the choices are rendered.
            if approval.choice {
                self.info_dialog = Some(InfoDialog::new(
                    "Answer in the Structured View",
                    "This request offers several answers, not allow/deny. Open the structured view to pick one.",
                ));
                return;
            }
            self.permission_response_dialog =
                Some(crate::tui::dialogs::PermissionResponseDialog::structured(
                    &title,
                    &approval.tool_name,
                    &approval.target,
                    approval.destructive,
                ));
            self.pending_permission_response = Some(PermissionResponseTarget::Structured {
                session_id: id,
                nonce: approval.nonce,
            });
            return;
        }
        let Some(response) = crate::agents::get_agent(&tool).and_then(|a| a.permission_response)
        else {
            self.info_dialog = Some(InfoDialog::new(
                "Not Supported",
                &format!("{} doesn't support quick permission responses yet.", tool),
            ));
            return;
        };
        self.pending_permission_response = Some(PermissionResponseTarget::Terminal(id));
        self.permission_response_dialog = Some(crate::tui::dialogs::PermissionResponseDialog::new(
            &title,
            response.allow_always,
        ));
    }

    /// Open the send-message dialog for the selected running session, draining
    /// pending_paste so dictation captured before a session was picked still gets used.
    /// No-op when no running session is targetable. Honors `view_mode`: in Terminal view
    /// it targets the paired terminal pane, so 'm' composes for the shell on screen.
    fn open_send_message_dialog(&mut self) {
        let Some((id, title, target)) = self.resolve_send_target() else {
            return;
        };
        let label = live_send::format_target_label(&title, &target);
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

    /// Compose target for the current view: agent in Structured view, the paired
    /// host/container terminal in Terminal view. Tool view has no clean target (the tool
    /// owns the pane), so it falls through to Agent for the historical capture path.
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

    /// Resolve `(id, title, target)` for an untargeted paste, 'm', or strict-mode letter
    /// capture. Agent targets keep the historical gate that the pane must already exist,
    /// so the dialog can't open against a stopped session; terminal targets relax it,
    /// since `execute_send_message` spawns the paired terminal on demand.
    fn resolve_send_target(&self) -> Option<(String, String, live_send::LiveSendTarget)> {
        let id = self.selected_session.as_ref()?;
        let inst = self.get_instance(id)?;
        if matches!(inst.status, Status::Creating | Status::Deleting) {
            return None;
        }
        let target = self.current_send_target();
        let ready = match &target {
            live_send::LiveSendTarget::Agent => crate::tmux::Session::new(&inst.id, &inst.title)
                .map(|s| s.exists())
                .unwrap_or(false),
            live_send::LiveSendTarget::Terminal | live_send::LiveSendTarget::ContainerTerminal => {
                true
            }
            live_send::LiveSendTarget::Tool(_) => true,
        };
        if !ready {
            return None;
        }
        Some((inst.id.clone(), inst.title.clone(), target))
    }

    /// Strict-mode typing guard: a bare lowercase letter outside navigation is treated as
    /// inadvertent typing and opens the compose dialog pre-filled with it. Mirrors
    /// handle_paste's delegation and fallback.
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
            let label = live_send::format_target_label(&title, &target);
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
    fn search_haystack_for(inst: &crate::session::Instance) -> String {
        format!("{} {}", inst.title, inst.project_path)
    }

    /// Rebuild `flat_items` and re-score any committed `search_matches` against the new
    /// indices. Every site that touches `self.flat_items` must go through this or
    /// `search_matches` keeps stale indices, and `n`/`N` jumps to the wrong sessions
    /// (#2676).
    pub(super) fn rebuild_flat_items(&mut self) {
        self.flat_items = self.build_flat_items();
        if !self.search_matches.is_empty() {
            self.refresh_search_matches();
        }
    }

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
                        Self::search_haystack_for(inst)
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
                        Self::search_haystack_for(inst)
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

    /// Gate sandbox creation on a one-time confirmation when the resolved config has glob
    /// `volume_ignores`: those are expanded against the workspace at create time, a
    /// snapshot that won't shadow directories a build creates later inside the container
    /// (#2045). Shown once, unless acknowledged or no glob is configured.
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
        if let Err(e) = update_app_state(|state| {
            state.has_acknowledged_volume_ignores_globs = true;
        }) {
            tracing::warn!(target: "tui.input", "Failed to save volume_ignores ack: {e}");
        }
    }

    /// Build the confirm message describing how this session's glob volume_ignores
    /// will expand, or `None` when there is no glob entry (nothing to confirm).
    fn volume_ignores_glob_confirm_message(data: &NewSessionData) -> Option<String> {
        let config =
            repo_config::resolve_config_with_repo(&data.profile, std::path::Path::new(&data.path))
                .ok()?;
        let expansions = crate::session::config::container_config::preview_glob_volume_ignores(
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
                let fallback = repo_config::ResolvedHooks::global(&data.profile);
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

        // Hooks to run if approved (repo hooks, else global) vs skipped: already-trusted
        // repo hooks still run, while newly-prompted ones fall back to the global set.
        let repo_root = std::path::Path::new(&trust.project_path);
        let hooks_on_trust = match &repo_hooks {
            Some(h) => repo_config::ResolvedHooks::with_repo(&data.profile, repo_root, h.clone()),
            None => repo_config::ResolvedHooks::global(&data.profile),
        };
        let hooks_on_skip = match &trust.hooks {
            TrustSurface::Trusted(h) => {
                repo_config::ResolvedHooks::with_repo(&data.profile, repo_root, h.clone())
            }
            _ => repo_config::ResolvedHooks::global(&data.profile),
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

    /// Create a session with optional hooks, delegating to the background
    /// `CreationPoller` when hooks are present, the session is sandboxed, or a worktree
    /// branch is requested, so a slow `post-checkout` can't freeze the TUI.
    pub(super) fn create_session_with_hooks(
        &mut self,
        data: NewSessionData,
        hooks: Option<repo_config::ResolvedHooks>,
    ) -> Option<Action> {
        let has_hooks = hooks
            .as_ref()
            .is_some_and(|h| !h.hooks().on_create.is_empty() || !h.hooks().on_launch.is_empty());
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
    fn wheel_mouse_bytes_sgr_maps_cell_and_button() {
        use ratatui::layout::Rect;
        // Pane at (10,5), 80x24. Wheel up over screen cell (12,7) maps to
        // 1-based pane cell (3,3): cx = 12-10+1, cy = 7-5+1.
        let pane = Rect::new(10, 5, 80, 24);
        assert_eq!(
            wheel_mouse_bytes(true, true, pane, 12, 7),
            b"\x1b[<64;3;3M".to_vec()
        );
        // Wheel down flips the button to 65.
        assert_eq!(
            wheel_mouse_bytes(false, true, pane, 12, 7),
            b"\x1b[<65;3;3M".to_vec()
        );
        // A cell past the pane edge clamps to the last column/row.
        assert_eq!(
            wheel_mouse_bytes(true, true, pane, 999, 999),
            b"\x1b[<64;80;24M".to_vec()
        );
        // An unpopulated rect falls back to the top-left cell.
        assert_eq!(
            wheel_mouse_bytes(true, true, Rect::new(0, 0, 0, 0), 40, 40),
            b"\x1b[<64;1;1M".to_vec()
        );
    }

    #[test]
    fn wheel_mouse_bytes_legacy_encodes_x10() {
        use ratatui::layout::Rect;
        let pane = Rect::new(10, 5, 80, 24);
        // Legacy X10: ESC [ M then (button+32, col+32, row+32). Cell (3,3),
        // wheel up (button 64) => 0x60, 0x23, 0x23.
        assert_eq!(
            wheel_mouse_bytes(true, false, pane, 12, 7),
            vec![0x1b, b'[', b'M', 64 + 32, 3 + 32, 3 + 32]
        );
        // Wheel down => button 65 => 0x61.
        assert_eq!(
            wheel_mouse_bytes(false, false, pane, 12, 7),
            vec![0x1b, b'[', b'M', 65 + 32, 3 + 32, 3 + 32]
        );
        // Coordinates above 223 can't be encoded in one byte; clamp there.
        let wide = Rect::new(0, 0, 400, 400);
        assert_eq!(
            wheel_mouse_bytes(true, false, wide, 300, 300),
            vec![0x1b, b'[', b'M', 64 + 32, 223 + 32, 223 + 32]
        );
    }

    #[test]
    fn mouse_event_bytes_sgr_press_release_and_drag() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 80, 24);
        // Cell (10,5) maps to 1-based (11,6). SGR: press `M`, release `m`,
        // keeping the button identity; a drag adds the motion bit (+32).
        assert_eq!(
            mouse_event_bytes(0, false, false, true, pane, 10, 5),
            b"\x1b[<0;11;6M"
        ); // left press
        assert_eq!(
            mouse_event_bytes(0, true, false, true, pane, 10, 5),
            b"\x1b[<0;11;6m"
        ); // left release
        assert_eq!(
            mouse_event_bytes(2, false, false, true, pane, 10, 5),
            b"\x1b[<2;11;6M"
        ); // right press
        assert_eq!(
            mouse_event_bytes(0, false, true, true, pane, 10, 5),
            b"\x1b[<32;11;6M"
        ); // left drag (0 + motion 32)
    }

    #[test]
    fn mouse_event_bytes_x10_press_button_release_agnostic() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 80, 24);
        // Legacy X10: ESC [ M then (button+32, cx+32, cy+32). Cell (10,5) =>
        // (11,6). A release is the button-agnostic 3.
        assert_eq!(
            mouse_event_bytes(0, false, false, false, pane, 10, 5),
            vec![0x1b, b'[', b'M', 32, 11 + 32, 6 + 32]
        ); // left press
        assert_eq!(
            mouse_event_bytes(0, true, false, false, pane, 10, 5),
            vec![0x1b, b'[', b'M', 3 + 32, 11 + 32, 6 + 32]
        ); // release => button 3
        assert_eq!(
            mouse_event_bytes(0, false, true, false, pane, 10, 5),
            vec![0x1b, b'[', b'M', 32 + 32, 11 + 32, 6 + 32]
        ); // left drag => button 0 + motion bit 32
    }

    fn cursor_for(
        alternate_on: bool,
        mouse_tracking: bool,
        mouse_sgr: bool,
    ) -> crate::tmux::PaneCursor {
        crate::tmux::PaneCursor {
            x: 0,
            y: 0,
            visible: true,
            pane_height: 24,
            history_size: 0,
            pane_width: 80,
            alternate_on,
            mouse_tracking,
            mouse_sgr,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        }
    }

    /// `hover_forward_bytes` fires only for a full-screen app in any-event tracking
    /// (1003) and then emits the no-button motion report in the app's encoding; everything
    /// else, a button-tracking app included, gets `None`.
    #[test]
    fn hover_forward_bytes_requires_any_event_tracking() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 80, 24);
        let mut all = cursor_for(true, true, true);
        all.mouse_all = true;
        // Cell (10,5) maps to 1-based (11,6); no-button motion is 3 + 32.
        assert_eq!(
            hover_forward_bytes(&all, pane, 10, 5).as_deref(),
            Some(b"\x1b[<35;11;6M".as_slice())
        );
        // Legacy X10 encoding still gets the motion report.
        all.mouse_sgr = false;
        assert_eq!(
            hover_forward_bytes(&all, pane, 10, 5),
            Some(vec![0x1b, b'[', b'M', 35 + 32, 11 + 32, 6 + 32])
        );
        // Button-only tracking: no bare motion.
        assert_eq!(
            hover_forward_bytes(&cursor_for(true, true, true), pane, 10, 5),
            None
        );
        // Normal screen: never forwarded, even with 1003 set.
        let mut normal = cursor_for(false, true, true);
        normal.mouse_all = true;
        assert_eq!(hover_forward_bytes(&normal, pane, 10, 5), None);
    }

    /// On a composited preview the rect is the whole window while input goes to pane 0
    /// alone, so a pointer over a neighbouring pane must not map against the full rect,
    /// which reported a column past pane 0's right edge. Also round-trips a painted
    /// composite cursor cell through mouse mapping, pinning the bottom-follow clipping.
    #[test]
    fn composited_preview_maps_the_mouse_into_pane_zero_only() {
        use ratatui::layout::Rect;
        // An 80x24 preview showing a window split at column 40.
        let pane = Rect::new(0, 0, 80, 24);
        let mut split = cursor_for(true, true, true);
        split.mouse_all = true;
        split.composite_pane0 = Some(crate::tmux::PaneGeom {
            left: 0,
            top: 0,
            width: 40,
            height: 24,
        });

        // Inside pane 0: maps as before, 1-based.
        assert_eq!(
            hover_forward_bytes(&split, pane, 10, 5).as_deref(),
            Some(b"\x1b[<35;11;6M".as_slice())
        );
        // Over the neighbour: dropped, not clamped to pane 0's border, which
        // would synthesise a hover on a cell the pointer never touched.
        assert_eq!(hover_forward_bytes(&split, pane, 60, 5), None);
        assert_eq!(wheel_forward_key(&split, true, pane, 60, 5), None);
        // The last column of pane 0 is still inside it; the first past it is not.
        assert!(hover_forward_bytes(&split, pane, 39, 5).is_some());
        assert_eq!(hover_forward_bytes(&split, pane, 40, 5), None);

        // In the bottom-follow layout the composite's top border row is clipped before
        // painting, so `first_line == pane0.top == 1`. The cursor mapper adds `top` after
        // its anchor delta while the mouse rect stays at the visible output origin, so a
        // click on the painted cursor must round-trip to that cursor's 1-based app cell.
        let mut bottom_follow = cursor_for(true, true, true);
        bottom_follow.x = 10;
        bottom_follow.y = 4;
        bottom_follow.pane_height = 25;
        bottom_follow.composite_pane0 = Some(crate::tmux::PaneGeom {
            left: 0,
            top: 1,
            width: 40,
            height: 24,
        });
        let painted = crate::tui::home::render::map_live_preview_cursor(
            pane,
            usize::from(pane.height),
            25,
            bottom_follow,
        )
        .expect("visible pane cursor");
        assert_eq!(
            map_pane_cell(mouse_pane_rect(&bottom_follow, pane), painted.x, painted.y,),
            (bottom_follow.x + 1, bottom_follow.y + 1),
            "clicking the painted cursor must report the same app cell"
        );

        // A no-mouse full-screen agent gets no page key from a wheel aimed at
        // the neighbour either, but keeps it over pane 0.
        let mut no_mouse = cursor_for(true, false, false);
        no_mouse.composite_pane0 = Some(crate::tmux::PaneGeom {
            left: 0,
            top: 0,
            width: 40,
            height: 24,
        });
        assert_eq!(wheel_forward_key(&no_mouse, true, pane, 60, 5), None);
        assert!(wheel_forward_key(&no_mouse, true, pane, 10, 5).is_some());

        // Unsplit is unchanged: no composite extent, so the whole rect maps.
        let unsplit = {
            let mut c = cursor_for(true, true, true);
            c.mouse_all = true;
            c
        };
        assert_eq!(
            hover_forward_bytes(&unsplit, pane, 60, 5).as_deref(),
            Some(b"\x1b[<35;61;6M".as_slice())
        );
        // An unsplit cell outside the rect must still clamp, not drop: the press is gated
        // by `hit_preview` and these helpers have always relied on `map_pane_cell`, so the
        // pane-0 containment test must not leak into the single-pane path.
        assert_eq!(
            hover_forward_bytes(&unsplit, pane, 999, 999).as_deref(),
            Some(b"\x1b[<35;80;24M".as_slice()),
            "unsplit coordinates clamp to the rect, they do not get dropped"
        );
    }

    /// A pane 0 extent larger than the preview rect (an attached client keeping its own
    /// size) must clamp to the rect rather than admit cells outside it.
    #[test]
    fn composite_pane_rect_clamps_to_the_preview() {
        use ratatui::layout::Rect;
        let pane = Rect::new(2, 3, 20, 10);
        let mut cursor = cursor_for(true, true, true);
        cursor.composite_pane0 = Some(crate::tmux::PaneGeom {
            left: 0,
            top: 0,
            width: 999,
            height: 999,
        });
        assert_eq!(mouse_pane_rect(&cursor, pane), pane);
        // And the origin is honored: a cell above/left of the rect is outside.
        assert_eq!(mouse_target_rect(&cursor, pane, 1, 3), None);
        assert!(mouse_target_rect(&cursor, pane, 2, 3).is_some());
    }

    /// The fix for #2407: a full-screen pane with no mouse tracking must forward
    /// `PageUp`/`PageDown`, not arrow keys (read as cursor navigation) and not raw mouse
    /// bytes. Asserting the key variant catches a regression the preview-offset test
    /// cannot.
    #[test]
    fn wheel_forward_key_no_mouse_alt_screen_is_page_key() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 80, 24);
        let cursor = cursor_for(true, false, false);
        assert_eq!(
            wheel_forward_key(&cursor, true, pane, 10, 10),
            Some(live_send::TmuxKey::NamedRepeat {
                name: "PageUp".into(),
                count: WHEEL_PAGE_STEP,
            })
        );
        assert_eq!(
            wheel_forward_key(&cursor, false, pane, 10, 10),
            Some(live_send::TmuxKey::NamedRepeat {
                name: "PageDown".into(),
                count: WHEEL_PAGE_STEP,
            })
        );
    }

    /// A mouse-tracking full-screen pane still gets a forwarded mouse event
    /// (SGR or legacy X10 bytes), never arrow keys.
    #[test]
    fn wheel_forward_key_mouse_tracking_is_hex_bytes() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 80, 24);
        match wheel_forward_key(&cursor_for(true, true, true), true, pane, 10, 10) {
            Some(live_send::TmuxKey::HexBytes(b)) => assert_eq!(b[0], 0x1b),
            other => panic!("expected SGR HexBytes, got {other:?}"),
        }
        match wheel_forward_key(&cursor_for(true, true, false), true, pane, 10, 10) {
            Some(live_send::TmuxKey::HexBytes(b)) => {
                assert_eq!(&b[..3], &[0x1b, b'[', b'M'])
            }
            other => panic!("expected legacy HexBytes, got {other:?}"),
        }
    }

    /// A normal-screen pane is never forwarded; the caller keeps the
    /// capture-window scroll, which can reach real scrollback there.
    #[test]
    fn wheel_forward_key_normal_screen_is_none() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 80, 24);
        assert_eq!(
            wheel_forward_key(&cursor_for(false, false, false), true, pane, 10, 10),
            None
        );
        assert_eq!(
            wheel_forward_key(&cursor_for(false, true, true), true, pane, 10, 10),
            None
        );
    }

    #[test]
    fn format_target_label_distinguishes_terminal_panes() {
        // Firing 'm' from Terminal view should show the target pane in the dialog title
        // and the live banner, so agent prompts don't land in a shell. Both route through
        // the same helper so the label can't drift.
        use live_send::{format_target_label, LiveSendTarget};
        assert_eq!(
            format_target_label("my-session", &LiveSendTarget::Agent),
            "my-session",
        );
        assert_eq!(
            format_target_label("my-session", &LiveSendTarget::Terminal),
            "my-session (terminal)",
        );
        assert_eq!(
            format_target_label("my-session", &LiveSendTarget::ContainerTerminal),
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
                background: false,
            },
        );
        tools.insert(
            "yazi".to_string(),
            ToolSessionConfig {
                command: "yazi".into(),
                hotkey: Some("Ctrl+f".into()),
                background: false,
            },
        );
        tools.insert(
            "tig".to_string(),
            ToolSessionConfig {
                command: "tig".into(),
                hotkey: Some("Alt+too-long".into()),
                background: false,
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
                background: false,
            },
        );
        tools.insert(
            "rg".to_string(),
            ToolSessionConfig {
                command: "rg --files".into(),
                hotkey: None,
                background: false,
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
                background: false,
            },
        );
        tools.insert(
            "lazygit".to_string(),
            ToolSessionConfig {
                command: "lazygit".into(),
                hotkey: Some("Alt+g".into()),
                background: false,
            },
        );
        tools.insert(
            "broken".to_string(),
            ToolSessionConfig {
                command: "x".into(),
                hotkey: Some("Ctrl+x".into()),
                background: false,
            },
        );
        tools.insert(
            "no-hotkey".to_string(),
            ToolSessionConfig {
                command: "y".into(),
                hotkey: None,
                background: false,
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
                background: false,
            },
        );
        tools.insert(
            "alpha".to_string(),
            ToolSessionConfig {
                command: "a".into(),
                hotkey: Some("Alt+g".into()),
                background: false,
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
            fork_seed: None,
            structured: false,
        }
    }

    /// The confirm gate only fires when the resolved config has a glob ignore
    /// that the message can name; literal-only ignores produce no dialog.
    #[test]
    #[serial_test::serial]
    fn volume_ignores_glob_confirm_message_fires_only_on_globs() {
        // `isolate_home` restores HOME/XDG on Drop (a bare `set_var` leaked the deleted
        // tempdir into later tests) and holds the process-global env lock meanwhile.
        let temp_home = tempfile::TempDir::new().unwrap();
        let _home = crate::session::test_support::isolate_home(temp_home.path());

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
