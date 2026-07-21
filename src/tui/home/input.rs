//! Input handling for HomeView

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::Position;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::bindings::{self, ActionId};
use super::{live_send, DragKind, HomeView, PreviewSelection, TerminalMode, ViewMode};
use crate::session::config::{
    load_config, update_app_state, update_config, GroupByMode, SortOrder,
};
use crate::session::{list_profiles, repo_config, resolve_config_or_warn, Item, Status};
use crate::tui::app::Action;
#[cfg(feature = "serve")]
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

/// Maximum gap between two left-clicks on the same row that still
/// counts as a double-click. 400ms matches the default on most desktop
/// environments. Worth tuning if real-world feedback says it's too
/// fast for trackpads or too slow on remote sessions.
const DOUBLE_CLICK_THRESHOLD: std::time::Duration = std::time::Duration::from_millis(400);

/// The two synthetic bottom-of-sidebar sections. Their headers look like
/// groups in `flat_items` but carry sentinel paths, so the section-scoped
/// context-menu actions (Restore All, collapse) resolve which one the cursor
/// is on through this rather than duplicating the path checks per call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SidebarSection {
    Trash,
    Archived,
}

/// Persist the user's picks from the first-run intro wizard. Theme name goes
/// to `config.theme.name`; attach mode goes to `default_attach_mode`, which
/// covers both Enter/double-click activation and the post-create attach.
/// Failures are logged and swallowed: the intro should never block startup
/// on a config write hiccup.
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

/// Persist the user's answer to the standalone telemetry consent popup.
/// Sets the opt-in flag, marks the prompt answered so it never re-appears,
/// and reconciles the install id (no-op under `DO_NOT_TRACK`).
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

/// Map a hovered screen cell `(col, row)` into a forwarded app's 1-based
/// mouse coordinate space, relative to its pane `pane` (the live-send target
/// is sized to the preview output rect, so this maps directly), clamped
/// inside the pane. An unpopulated rect falls back to the top-left cell.
/// Shared by the wheel- and click-forward byte builders.
fn map_pane_cell(pane: ratatui::layout::Rect, col: u16, row: u16) -> (u16, u16) {
    if pane.width == 0 || pane.height == 0 {
        (1u16, 1u16)
    } else {
        let cx = col.saturating_sub(pane.x).min(pane.width - 1) + 1;
        let cy = row.saturating_sub(pane.y).min(pane.height - 1) + 1;
        (cx, cy)
    }
}

/// Build the mouse-wheel byte sequence to forward to a full-screen app
/// under the live preview. `up` selects wheel-up (button 64) vs wheel-down
/// (65); `sgr` selects the SGR (1006) encoding vs the legacy X10 encoding,
/// matching whatever the app has enabled.
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
        // Legacy X10: `ESC [ M` then three bytes, each the value + 32.
        // Bytes top out at 255, so coordinates above 223 can't be
        // encoded; clamp there (preview cells are far below that anyway).
        let enc = |v: u16| (v.min(223) + 32) as u8;
        vec![0x1b, b'[', b'M', enc(button), enc(cx), enc(cy)]
    }
}

/// Build the byte sequence for a single forwarded mouse button event at
/// screen cell `(col, row)`, mapped into the app's pane. `base_button` is the
/// SGR low-bits code (left=0, middle=1, right=2); `release` is a button-up;
/// `motion` is a drag (button held while moving). `sgr` picks SGR (1006) vs
/// the legacy X10 encoding, matching the app. Mirrors `wheel_mouse_bytes`,
/// which is just the wheel buttons (64/65).
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

/// The bare mouse-motion (hover) bytes to forward to the previewed pane, or
/// `None` when the app didn't ask for them. Only a full-screen app in
/// any-event tracking (DEC 1003, tmux `#{mouse_all_flag}`) gets bare motion:
/// a button-tracking (1000/1002) app never expects a no-button motion report.
/// The report is the button-agnostic code 3 plus the motion bit (SGR `35`),
/// which is what a real terminal emits for hover, so an app that highlights
/// content under the pointer (Claude Code's expandable-block hover) reacts in
/// the live preview the way it does over a direct tmux attach. Pure so the
/// gate is asserted directly in tests, mirroring `wheel_forward_key`.
fn hover_forward_bytes(
    cursor: &crate::tmux::PaneCursor,
    pane: ratatui::layout::Rect,
    col: u16,
    row: u16,
) -> Option<Vec<u8>> {
    (cursor.alternate_on && cursor.mouse_all)
        .then(|| mouse_event_bytes(3, false, true, cursor.mouse_sgr, pane, col, row))
}

/// Page presses delivered per wheel notch for a no-mouse full-screen app.
/// One page per notch: full-screen apps that scroll on `PageUp`/`PageDown`
/// (Claude Code's fullscreen renderer is the motivating case) have no finer
/// keyboard step, and a page per notch reads as a normal "flick to scroll".
const WHEEL_PAGE_STEP: usize = 1;

/// Decide what to forward to the previewed full-screen pane for one wheel
/// notch, or `None` to fall back to the capture-window scroll. Pure so the
/// branch the fix turns on (page keys vs. raw mouse bytes vs. no forward) is
/// asserted directly, without standing up a worker. See
/// `forward_wheel_to_preview` for the full rationale.
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
    if cursor.mouse_tracking {
        Some(live_send::TmuxKey::HexBytes(wheel_mouse_bytes(
            up,
            cursor.mouse_sgr,
            pane,
            col,
            row,
        )))
    } else {
        // No mouse tracking: send `PageUp`/`PageDown`, NOT arrow keys. A
        // full-screen app reads arrow keys as cursor / input-history
        // navigation, not scroll (Claude Code 2.1.x even surfaces "Scroll
        // wheel is sending arrow keys, use PgUp/PgDn to scroll"). The page
        // keys scroll its transcript regardless of cursor-key mode.
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

    /// Whether the full-screen Settings takeover is showing. The wheel
    /// scroll gate in `app.rs` uses this so the whole screen counts as a
    /// scroll target (the fields panel is the only scrollable surface,
    /// but the list/preview hit rects it replaced are now stale).
    pub fn is_settings_open(&self) -> bool {
        self.settings_view.is_some()
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

    /// The `KeyEvent` a footer-toolbar button at `(col, row)` synthesizes,
    /// or `None` when the click misses every button. The caller routes the
    /// returned key through the normal key handler so a click behaves
    /// exactly like pressing the shortcut. Returns `None` while a non-live
    /// overlay (dialog, context menu, help, search) is open: the footer is
    /// drawn underneath it, but the overlay owns clicks, so a footer button
    /// must not fire a shortcut behind it.
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

    /// Handle a left-click on the sidebar collapse/expand affordances:
    /// the collapse button on the expanded list's top-right border, or
    /// anywhere in the collapsed strip. Returns true when the click landed
    /// on one (and toggled), so the caller can stop before the row-click
    /// path. The collapse button sits on the list's top border, which is
    /// inside `list_area`, so this MUST run before `hit_list` or the click
    /// falls through to `handle_empty_list_click` and opens a new session.
    /// No-op while a non-live overlay is open so a click can't toggle the
    /// sidebar behind a modal (the rects are cleared for the full-screen
    /// takeover views, so a dialog overlay is the case left to guard).
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

    /// Click on the footer tips badge: open the tips overlay. Returns true when
    /// the click was on the badge (so the caller stops routing it). Gated on no
    /// overlay being open, the badge rect is captured behind any modal, so this
    /// keeps a click in the bottom-right corner from punching through.
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
        // Settings scrollbar grab: map the live row to a scroll offset.
        // Handled before the cancel-on-overlay logic below, which keys
        // off `has_dialog()` (true while settings is open) and would
        // otherwise tear the drag down on the first move.
        if matches!(self.drag_state, Some(DragKind::SettingsScrollbar)) {
            if let Some(view) = &mut self.settings_view {
                return view.scrollbar_drag_to_row(row);
            }
            // Settings closed mid-drag (shouldn't happen while the button
            // is held); drop the stale drag so the next Up is a no-op.
            self.drag_state = None;
            return false;
        }
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
            // Handled by the early return at the top of this function.
            Some(DragKind::SettingsScrollbar) => false,
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
        // Pace the scroll to a steady cadence regardless of how often the loop
        // woke this iteration, so the speed is even instead of racing with
        // capture-worker activity. The aoe-side line scroll and the wheel
        // forward to a mouse agent are both fine-grained (one line / one wheel
        // notch), so they run at the fast cadence and read as smooth, like the
        // native wheel forward when the live pane is active. The no-mouse
        // page-key fallback stays slow: each press scrolls a WHOLE page, so a
        // held edge at the fast cadence would rocket through the transcript.
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
                // Pin the extent to the now-revealed edge line in `from_bottom`
                // terms, which the new scroll offset gives directly: the bottom
                // visible line sits `offset` lines up from the newest line, the
                // top visible line `offset + height - 1`. Deriving it from the
                // offset (not the stale pre-scroll `total_lines`) keeps it
                // correct even before the next frame re-captures.
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
        // The capture-window scroll is inert: a full-screen (alternate-screen)
        // agent has no aoe-side scrollback, so forward the same scroll input a
        // wheel notch would to the agent instead, scrolling its OWN transcript
        // the way the wheel forward does (#2421). The extent stays pinned to the
        // screen edge; the agent's redraw is what reveals more text under the
        // held selection.
        if forward_ready && self.forward_scroll_to_preview(at_top, col, row) {
            self.preview_autoscroll_at = Some(now);
            return true;
        }
        false
    }

    /// Shift the preview scroll offset by `delta` lines (positive scrolls
    /// up toward older output), clamped to the captured window. Returns
    /// whether the offset actually moved. Factored out of the wheel
    /// handlers so the edge auto-scroll can move the pane without dragging
    /// the whole `handle_scroll_*` routing along with it.
    fn scroll_preview_offset(&mut self, delta: i32) -> bool {
        // Use the same rendered output-body height the per-frame clamp uses
        // (`clamp_scroll_to_capture`), NOT `dimensions.1 - 1`. The raw-height
        // `- 1` over-counts the max offset by a row, so on an alternate-screen
        // agent with no scrollback this would grant a phantom 1-row offset that
        // render erases every frame: the offset oscillates 0->1->0, the view
        // never moves, and because it returns `true` the caller never falls
        // through to the agent scroll-forward fallback.
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
            // The offset was applied live on each move; the release just
            // ends the gesture. Nothing to persist (scroll is view state).
            DragKind::SettingsScrollbar => {}
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
        // The structured preview's transcript is its own line source: the
        // view re-derives the same pre-wrapped rows the renderer painted
        // (see `wrapped_transcript`), so (row, column) slicing below maps
        // one-to-one onto the on-screen cells. Terminal previews keep
        // reading the tmux capture cache.
        #[cfg(feature = "serve")]
        let structured_lines = self
            .structured_preview
            .as_ref()
            .filter(|v| {
                self.selected_session
                    .as_deref()
                    .is_some_and(|id| id == v.session_id())
            })
            .map(|v| v.selection_text(width));
        #[cfg(feature = "serve")]
        let lines = match structured_lines.as_ref() {
            Some(text) => text,
            None => self.active_preview_cache().parsed_text.as_ref()?,
        };
        #[cfg(not(feature = "serve"))]
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
    /// actions (`delete_group`, `force_remove_session`, `trash_session`) run inline and
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
            #[cfg(feature = "serve")]
            "switch_view" => self
                .pending_switch_view_session
                .take()
                .map(Action::SwitchSessionView),
            #[cfg(feature = "serve")]
            "start_daemon_structured" => self
                .pending_daemon_start_session
                .take()
                .map(Action::StartDaemonThenOpenStructured),
            "quit_during_creation" => Some(Action::Quit),
            "quit" => Some(Action::Quit),
            _ => None,
        }
    }

    /// Open a neutral confirm dialog offering to pull the newer sandbox image
    /// the registry check surfaced. The image is stashed in
    /// `pending_image_pull` because the generic `ConfirmDialog` only carries an
    /// action string; the Submit handler reads it back to emit the pull.
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

    /// Confirm before permanently purging every trashed session (the Trash
    /// section's "Empty Trash" bulk action). The purge is irreversible, so it
    /// keeps the destructive red tone rather than the neutral archive one. A
    /// no-op info dialog when the trash is already empty avoids a confirm that
    /// would delete nothing.
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
                        // `clear_terminal` (the close-flash). Same
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
                        self.pending_stop_terminal = None;
                        self.pending_stop_tool = None;
                        self.pending_force_remove_session = None;
                        self.pending_trash_session = None;
                        self.pending_image_pull = None;
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
            // A press on the fields-panel scrollbar begins a grab-drag:
            // seed the drag state and jump to the pressed row so the
            // subsequent Drag(Left) events map to the bar. Must precede
            // handle_click so the bar column isn't treated as a field
            // miss. The drag_state assignment ends the `view` borrow, so
            // this is split into a hit test and a follow-up.
            if view.hit_scrollbar(col, row) {
                view.scrollbar_drag_to_row(row);
                self.drag_state = Some(DragKind::SettingsScrollbar);
                return true;
            }
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
                        if let Err(e) = crate::session::config::update_app_state(|state| {
                            state.has_acknowledged_agent_hooks = true;
                        }) {
                            tracing::warn!(target: "tui.input", "Failed to save config: {e}");
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
                    // which forces a `clear_terminal` on the next loop
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
                    self.hooks_install_dialog = None;
                    // Persist the acknowledgment
                    if let Err(e) = crate::session::config::update_app_state(|state| {
                        state.has_acknowledged_agent_hooks = true;
                    }) {
                        tracing::warn!(target: "tui.input", "Failed to save config: {e}");
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
                        // Backing out of `n` with a selection is the most
                        // contextual moment to surface the new-from-selection
                        // tip; queue it (no-op until it's earned). On submit we
                        // skip the pop so it doesn't interrupt session creation;
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

        if let Some(dialog) = &mut self.plugin_manager_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel | DialogResult::Submit(()) => {
                    self.plugin_manager_dialog = None;
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

        // Permission response dialog
        if let Some(dialog) = &mut self.permission_response_dialog {
            match dialog.handle_key(key) {
                DialogResult::Continue => {}
                DialogResult::Cancel => {
                    self.permission_response_dialog = None;
                    self.pending_permission_response_session = None;
                }
                DialogResult::Submit(choice) => {
                    self.permission_response_dialog = None;
                    if let Some(session_id) = self.pending_permission_response_session.take() {
                        self.execute_permission_response(&session_id, choice);
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
        // overlay-routing block above has returned, so nothing is open here.
        // Skip while searching so it can't interrupt a query. Opening the pop
        // consumes this keystroke (it's a one-time, dismissable nudge). #2262
        if !self.search_active && self.pending_tip_pop.is_some() && self.drain_pending_tip_pop() {
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
                    // vim-parity: Enter commits but keeps search_matches
                    // and search_query so `n`/`N` cycle survives reloads
                    // (`refresh_search_matches` re-scores against the same
                    // query). Esc above remains the cancel-and-clear path.
                    // See #2676.
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
            return self.activate_tool(tool_name, true);
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
            // Tab is the activation key's complement: it does whichever of
            // (live-send, tmux attach) `Enter` doesn't. Only fires when
            // `live_send` is None (the live-send capture short-circuits above).
            KeyCode::Tab => {
                // A structured session has neither a tmux pane to attach
                // nor one to live-send into; both Tab meanings are
                // vacuous. Say so instead of silently ignoring the press.
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
                // Post-#2676: committed matches from a prior `/`-search
                // persist across `Enter`; clear them here so `[i/N]` does
                // not briefly render over an empty input on reopen.
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
                if matches!(self.view_mode, ViewMode::Terminal) {
                    if let Some(action) = self.maybe_auto_start_live_send() {
                        return Some(action);
                    }
                }
            }
            ActionId::SendMessage => self.open_send_message_dialog(),
            ActionId::RespondToPermission => self.open_permission_response_dialog(),
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
            ActionId::Plugins => {
                self.plugin_manager_dialog = Some(crate::tui::dialogs::PluginManagerDialog::new());
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
            ActionId::SortPicker => self.show_sort_picker(),
            ActionId::GroupBy => self.show_group_picker(),
            ActionId::ToggleProjectPin => self.toggle_project_pin_at_cursor(),
            ActionId::NextWaiting => self.jump_to_next_waiting(),
            ActionId::Tips => self.open_tips_dialog(),
            ActionId::Fork => self.open_fork_from_selection(),
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
            // The user just used N, so they've discovered it; suppress the tip
            // that teaches it.
            self.record_used_new_from_selection();
        }
    }

    /// Whether the session `id` can be forked, so the context menu shows the
    /// "Fork session" row only when the palette action would succeed. A
    /// structured parent needs an agent advertising the ACP fork capability; a
    /// terminal parent needs a forkable terminal agent (claude/codex/opencode).
    /// The captured-conversation precondition is intentionally NOT checked here:
    /// the row still shows for a not-yet-started session, and the palette action
    /// explains "nothing to fork yet" if the user picks it, matching how other
    /// rows stay visible and explain on use.
    pub(super) fn session_can_fork(&self, id: &str) -> bool {
        let Some(inst) = self.get_instance(id) else {
            return false;
        };
        if inst.is_structured() {
            #[cfg(feature = "serve")]
            {
                crate::session::fork::structured_fork_capable(
                    &inst.tool,
                    inst.agent_name.as_deref(),
                )
            }
            #[cfg(not(feature = "serve"))]
            {
                false
            }
        } else {
            crate::session::fork::terminal_agent_can_fork(&inst.tool)
        }
    }

    /// Whether the session's persisted view can be switched, and which view
    /// it currently renders: `Some(is_structured)` when the context menu
    /// should offer a switch, `None` otherwise. A structured session can
    /// always go back to a terminal; a terminal session can go structured
    /// only when its tool is ACP-capable and the `offer_structured_in_new_session`
    /// opt-in is on (the structured view is still maturing, so switching into it
    /// is gated the same way the new-session toggle is). Serve builds only (the swap runs
    /// through the daemon and the result needs a structured view to open).
    /// Archived / trashed / mid-lifecycle rows are excluded: their agent is
    /// deliberately stopped, and the swap would boot a worker or tmux pane
    /// behind the parked state.
    pub(super) fn session_switch_view_target(&self, id: &str) -> Option<bool> {
        #[cfg(feature = "serve")]
        {
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
            let config = crate::session::repo_config::resolve_config_with_repo_or_warn(
                &inst.source_profile,
                std::path::Path::new(&inst.project_path),
            );
            // Switching a terminal session INTO the structured view is gated on
            // the same opt-in as the new-session toggle: the structured view is
            // still maturing, so don't offer to switch into it unless the user
            // turned it on. The reverse direction (already-structured -> terminal)
            // stays available above regardless, so a session can always get out.
            (config.acp.offer_structured_in_new_session
                && crate::session::builder::structured::tool_acp_capable(&inst.tool, &config))
            .then_some(false)
        }
        #[cfg(not(feature = "serve"))]
        {
            let _ = id;
            None
        }
    }

    /// Confirm before flipping the selected session's view. Enabling
    /// structured view destroys the tmux scrollback. Disabling destroys the
    /// ACP transcript context UNLESS the agent pairing is CLI-resumable
    /// (claude), in which case the conversation continues in the terminal via
    /// `--resume` and the copy says so (#2252). The id is stashed like the
    /// other confirm-carrying actions because `ConfirmDialog` only carries
    /// an action string.
    pub(super) fn prompt_switch_view_for_selected(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(to_structured) = self.session_switch_view_target(&id).map(|s| !s) else {
            return;
        };
        // Claude keeps context on the way back to the terminal (the switch
        // resumes `claude --resume`), so the warning must not threaten data
        // loss for it. Other agents still restart fresh. Mirror the backend
        // gate (agents::acp_transcript_cli_resumable) on the active adapter,
        // approximated here by agent_name (or the tool when unset). See #2252.
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

    /// Offer to start a localhost daemon when opening a structured view
    /// found no daemon running. Neutral tone: nothing is destroyed; the
    /// user is just about to run a background process they may not have
    /// expected. Remote setups keep their manual commands.
    #[cfg(feature = "serve")]
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

    /// The selected session's id when it is a structured-view row that
    /// can be previewed: not mid create/delete, and not parked in the
    /// archive or trash (those render their own placeholder pages, so a
    /// mounted view would be invisible while still streaming). Drives
    /// preview-on-select and the active-view liveness check.
    #[cfg(feature = "serve")]
    pub(in crate::tui) fn selected_structured_session(&self) -> Option<String> {
        let id = self.selected_session.clone()?;
        let inst = self.get_instance(&id)?;
        let previewable = inst.is_structured()
            && !matches!(inst.status, Status::Creating | Status::Deleting)
            && !inst.is_archived()
            && !inst.is_trashed();
        previewable.then_some(id)
    }

    /// Leave live-send mode if it is active, restoring pane sizing.
    /// Used by the embedded structured view open path: both modes route
    /// keystrokes away from the home view and paint the preview pane,
    /// so they cannot coexist.
    #[cfg(feature = "serve")]
    pub(in crate::tui) fn exit_live_send_if_active(&mut self) {
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
        }
    }

    /// Open the new-session dialog seeded as a fork of the selected session.
    /// The forked session resumes the parent's captured conversation in a fresh
    /// independent session id, so it can diverge without disturbing the parent.
    /// Inherits the parent's repo path and group like "new from selection";
    /// refuses (with an explanatory info dialog) when the agent can't fork or
    /// the parent has no captured conversation yet.
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

        // Pull the few parent fields we need into owned locals so the
        // immutable borrow of `self` is dropped before the mutable `self.`
        // calls below (dialog construction, info_dialog assignment). A
        // structured view parent forks through ACP. The captured ACP session
        // id is read into a local only under `serve`, since only that fork
        // branch consumes it.
        let Some(parent) = self
            .selected_session
            .as_ref()
            .and_then(|id| self.get_instance(id))
        else {
            return;
        };
        let tool = parent.tool.clone();
        let parent_agent_session_id = parent.agent_session_id.clone();
        let repo_path = parent.repo_path().to_string();
        let group_path = parent.group_path.clone();
        let title = parent.title.clone();
        let parent_is_structured = parent.is_structured();
        #[cfg(feature = "serve")]
        let parent_agent_name = parent.agent_name.clone();
        #[cfg(feature = "serve")]
        let parent_acp_session_id = parent.acp_session_id.clone();

        let seed = if parent_is_structured {
            // A structured parent forks via the ACP `session/fork` handshake,
            // not the terminal resume-with-fork-flag path. The captured ACP
            // session id is the parent to fork from; without one there is no
            // conversation to fork yet.
            #[cfg(feature = "serve")]
            {
                // Gate on the same predicate the REST create-guard and the web
                // `acp_can_fork` projection use: a resume-only ACP agent (e.g.
                // `aoe-agent`) has no fork strategy, so `session/fork` would be
                // refused at the handshake and silently downgrade to
                // `session/new`, handing the user an empty session they think is
                // a fork. Refuse up front with the same info dialog terminal
                // denial uses.
                if !crate::session::fork::structured_fork_capable(
                    &tool,
                    parent_agent_name.as_deref(),
                ) {
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
            }
            #[cfg(not(feature = "serve"))]
            {
                self.info_dialog = Some(InfoDialog::new(
                    "Fork not available in this build",
                    "This `aoe` binary was built without the web dashboard \
                     (a `--no-default-features` source build), so structured \
                     view session forking is not included.\n\n\
                     To fork this session:\n\
                       \u{2022} Install a release build from GitHub Releases, or\n\
                       \u{2022} Build from source with default features:\n\
                         cargo build --release",
                ));
                return;
            }
        } else {
            let child_id = crate::session::capture::generate_claude_session_id();
            match crate::session::fork::terminal_fork_seed(
                &tool,
                parent_agent_session_id.as_deref(),
                child_id,
            ) {
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
        let profiles = list_profiles().unwrap_or_else(|_| vec![current_profile.clone()]);
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
        // The seed forks the parent's agent, so the dialog must open on that
        // same agent rather than the configured default; otherwise a Codex or
        // OpenCode parent would land on claude and mismatch the seed.
        dialog.set_tool(&tool);
        dialog.set_fork_from(seed);
        dialog.focus_title();
        self.new_dialog = Some(dialog);
    }

    /// Pick a representative repo path for a selected group so "New Session"
    /// from a project/group can prefill the working directory. In project mode
    /// the group label is a derived basename, so match members by
    /// `project_group_name`; in manual mode match by the stored `group_path`,
    /// including nested subgroups. Returns `None` for an empty group (no member
    /// to borrow a path from), leaving the dialog on the default cwd.
    pub(super) fn group_repo_path(&self, group_path: &str) -> Option<String> {
        self.instances
            .values()
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

    pub(super) fn stop_selected(&mut self) {
        // Stop targets what the user is looking at. In Terminal view that is
        // the paired terminal, and in Tool view the tool session; killing the
        // agent (docker stop + status flip) from either would be surprising.
        // Only Agent (structured) view stops the agent session itself.
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

    /// Terminal-view Stop: confirm, then kill the paired terminal (host or
    /// container, whichever the row is showing) without touching the agent
    /// session. No-op when the terminal isn't running, so pressing Stop on an
    /// idle terminal row doesn't pop a pointless dialog.
    fn stop_terminal_selected(&mut self) {
        let Some(session_id) = self.selected_session.clone() else {
            return;
        };
        let Some(inst) = self.get_instance(&session_id) else {
            return;
        };
        let mode = if inst.is_sandboxed() {
            self.get_terminal_mode(&session_id)
        } else {
            TerminalMode::Host
        };
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

    /// Kill the paired terminal for `session_id` (host or container per `mode`),
    /// then refresh so the Terminal-view row drops back to its idle glyph. The
    /// agent session is left untouched.
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

    /// Tool-view Stop: confirm, then kill the tool session (lazygit, yazi,
    /// ...) without touching the agent session. Mirrors
    /// `stop_terminal_selected`: no-op when the tool isn't running.
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

        // A session on a non-git project runs in place, so there is no repo to
        // diff against. Show a clear message instead of letting the git layer
        // surface a raw "could not open repository" error.
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

    fn open_serve(&mut self) {
        #[cfg(feature = "serve")]
        {
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
        #[cfg(not(feature = "serve"))]
        {
            self.info_dialog = Some(InfoDialog::new(
                "Serve unavailable",
                "This `aoe` binary was built without the web dashboard \
                 (a `--no-default-features` source build), so local network \
                 serving and Cloudflare Tunnel integration are not included.\n\n\
                 To serve to your phone (LAN / Tailscale / tunnel):\n\
                   \u{2022} Install a release build from GitHub Releases, or\n\
                   \u{2022} Build from source with default features:\n\
                     cargo build --release\n\n\
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
                    // The synthetic Archived section header (and any
                    // sub-folder rendered under it in Project mode) is
                    // not a real group; skip it so the palette doesn't
                    // surface the sentinel path or invite Jump-to-group
                    // navigation that the rest of the codebase
                    // intentionally disarms.
                    if crate::session::is_within_archived_section(path)
                        || crate::session::is_within_trash_section(path)
                    {
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
                // any leftover search state first: otherwise picking "New
                // session" while a search is committed would route the
                // dual-purpose `n`/`N` actions into a search-cycle instead.
                // Also clear `search_query` (post-#2676) so a subsequent
                // rebuild path calling `refresh_search_matches` cannot
                // resurrect phantom matches against the stale query.
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
                // Trashed rows are stopped and only surface under the collapsed
                // Trash section; they never "need attention", so skip them even
                // when a stale unread flag survived the trash (#2489). Snoozed
                // and archived rows are likewise explicit "don't bother me"
                // sink states, same as everywhere else that checks
                // `is_snoozed()` / `is_archived()`.
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
            if inst.is_dismissed() {
                continue;
            }
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
            let id = match self.flat_items.get(idx) {
                Some(Item::Session { id, .. }) => id.clone(),
                _ => return,
            };
            self.jump_to_session_id(&id);
            return;
        }

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
                    // The embedded structured view takes over the preview
                    // pane; leave live-send first so the two don't fight
                    // over it (mirrors `exit_live_send_before_attach`).
                    self.exit_live_send_if_active();
                    return Some(Action::OpenStructuredView(id));
                }
                #[cfg(not(feature = "serve"))]
                {
                    return Some(Action::SetTransientStatus(
                        "Acp session: rebuild with default features to attach".to_string(),
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
                    Some(crate::session::AttachMode::LiveSend)
                ) {
                    self.start_live_send()
                } else {
                    self.exit_live_send_before_attach();
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
            // Structured rows never reach here: the Tab keybinding
            // refuses them with a "no tmux pane" toast before calling
            // this resolver.
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
                // A finalized preview selection pins to the previous pane's
                // cells; carried into a different session it would paint a
                // stale highlight and, because the preview holds its snapshot
                // while a selection is live, stop the new session's preview
                // from following output. Keystroke and click navigation already
                // drop it upstream; clearing here also covers programmatic
                // reselects (e.g. a poller) that never ran those paths.
                self.clear_preview_selection();
                // Moving off a hand-flagged row ends its manual-unread hold, so
                // returning to it later dwell-clears like any other unread. Done
                // here at the cursor->selection sync (every navigation path runs
                // through it) so the release doesn't hinge on a dwell tick
                // happening to fire during a quick hop to another row.
                self.manual_unread_hold = None;
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

    fn toggle_group_collapsed(&mut self, path: &str) {
        // The synthetic Archived section is not a member of any
        // GroupTree; its collapsed state lives on HomeView and persists
        // separately. Route here before either branch tries to mutate a
        // nonexistent group.
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

    /// Forward one wheel notch to the previewed full-screen (alternate-screen)
    /// pane so it scrolls its OWN content, exactly as a terminal does on
    /// direct attach. Active in BOTH live-send and passive preview: the
    /// alternate screen has no scrollback, so the preview's capture-window
    /// scroll is inert there (growing the window only exposes the unrelated
    /// normal-buffer history and bottoms out at the session start), and
    /// forwarding is the only way to reach the agent's history. Branched on
    /// what the app asked for (see `wheel_forward_key`):
    ///
    /// * **Mouse tracking on**: forward the wheel as a mouse event, encoding
    ///   following the app (SGR 1006 when `mouse_sgr` is set, else legacy
    ///   X10). The previewed pane is sized to the preview rect in both modes
    ///   (`preview_pane_synced` / the live-send sync resize), so the mapped
    ///   coordinates land inside it.
    /// * **Mouse tracking off**: send `PageUp`/`PageDown`, not arrow keys. A
    ///   full-screen app reads arrows as cursor / input-history navigation,
    ///   not scroll; the page keys scroll its transcript regardless of mode.
    ///   Claude Code's fullscreen renderer is the motivating case (#2407).
    ///
    /// Normal-buffer panes get `None` from `wheel_forward_key`, so the caller
    /// keeps the capture-window scroll (which reaches real scrollback there).
    ///
    /// In live-send the key rides the ordered `LiveSendWorker` so it stays in
    /// sequence with typed keystrokes. In passive preview there is no worker,
    /// so it goes out as a one-shot fork to the previewed pane's tmux target.
    /// Returns true when the event was forwarded.
    fn forward_wheel_to_preview(&self, up: bool, col: u16, row: u16) -> bool {
        let cursor = self
            .preview_capture_worker
            .as_ref()
            .and_then(|w| w.current_cursor());
        let Some(cursor) = cursor else { return false };
        let Some(key) = wheel_forward_key(&cursor, up, self.preview_text_view.pane, col, row)
        else {
            return false;
        };
        self.send_to_preview_pane(key)
    }

    /// During an edge-held preview selection over a full-screen
    /// (alternate-screen) agent, the aoe capture-window scroll is inert (the
    /// alternate screen has no scrollback, so `scroll_preview_offset` can't
    /// move), so forward the SAME scroll input one wheel notch would, scrolling
    /// the agent's OWN transcript. Mirrors `forward_wheel_to_preview` by reusing
    /// `wheel_forward_key`: a mouse-tracking app gets a wheel-up/down mouse
    /// report at the held cell (PageUp does nothing while it owns the mouse), a
    /// no-mouse app gets `PageUp`/`PageDown`. `wheel_forward_key` also enforces
    /// the alternate-screen gate, so a normal-buffer pane that has merely
    /// bottomed out its scrollback never gets scroll input injected into its
    /// shell. `up` selects the top-edge (scroll back) vs bottom-edge (scroll
    /// forward) direction; `col`/`row` is the held pointer cell, mapped into the
    /// pane for the mouse-byte encoding. Returns true when something was sent.
    fn forward_scroll_to_preview(&self, up: bool, col: u16, row: u16) -> bool {
        let Some(cursor) = self
            .preview_capture_worker
            .as_ref()
            .and_then(|w| w.current_cursor())
        else {
            return false;
        };
        let Some(key) = wheel_forward_key(&cursor, up, self.preview_text_view.pane, col, row)
        else {
            return false;
        };
        self.send_to_preview_pane(key)
    }

    /// Send a forwarded key/mouse-byte payload to the pane the preview is
    /// showing (`preview_capture_target`). The cursor and mapped coordinates
    /// describe THAT pane, so the bytes must go there. Route through the
    /// ordered live-send worker only when the displayed pane is also the
    /// live-send target, so the event stays in sequence with typed
    /// keystrokes; otherwise (passive preview, or live-send aimed at a
    /// different pane than the one on screen) fork a one-shot send. Returns
    /// true when something was dispatched.
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

    /// The previewed agent's cursor when a mouse button event over the preview
    /// should be forwarded to it instead of driving aoe's own UI: the pane must
    /// be a full-screen app with mouse tracking on (the same gate as the wheel's
    /// mouse-byte branch). Works in passive preview too, not just live-send,
    /// mirroring `forward_wheel_to_preview`: hovering a mouse agent and dragging
    /// drives its native selection/scroll, and `send_to_preview_pane` forks a
    /// one-shot send when there's no live-send worker. `None` means let the
    /// event fall through to aoe's handlers (selection, etc.); Shift is the
    /// caller's escape hatch back to aoe-side selection / copy. Returns the
    /// cursor so the caller can read `mouse_sgr` for the encoding.
    fn preview_forwards_mouse(&self) -> Option<crate::tmux::PaneCursor> {
        let cursor = self
            .preview_capture_worker
            .as_ref()
            .and_then(|w| w.current_cursor())?;
        (cursor.alternate_on && cursor.mouse_tracking).then_some(cursor)
    }

    /// Forward a mouse button event (press / release / drag) over the preview
    /// straight to the previewed agent, the way a direct tmux attach would.
    /// `Shift` is the escape hatch: a Shift-held event returns `false` so it
    /// falls through to aoe's own preview text-selection. Returns `true` when
    /// the event was forwarded (and should be consumed). Wheel and bare-motion
    /// events are not handled here; the wheel has its own path
    /// (`forward_wheel_to_preview`) and so does bare motion
    /// (`forward_hover_to_preview`).
    ///
    /// A button held down is tracked in `mouse_forward_btn` so its drag and
    /// release reach the agent even if the pointer leaves the preview rect
    /// mid-drag, which keeps the agent from seeing a stuck button.
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
        // The single mouse-forwarding gate, shared by press/drag/release: a
        // press over a non-mouse pane bails here, and a gesture whose pane
        // stopped being a mouse agent mid-drag drops its tracked button so we
        // don't strand it.
        let Some(cursor) = self.preview_forwards_mouse() else {
            self.mouse_forward_btn = None;
            return false;
        };
        self.mouse_forward_btn = if release { None } else { Some(base_button) };
        let bytes = mouse_event_bytes(
            base_button,
            release,
            motion,
            cursor.mouse_sgr,
            self.preview_text_view.pane,
            col,
            row,
        );
        self.send_to_preview_pane(live_send::TmuxKey::HexBytes(bytes))
    }

    /// Forward a bare mouse-motion event over the preview to a previewed
    /// agent in any-event tracking (DEC 1003), so its hover-driven UI (e.g.
    /// Claude Code highlighting an expandable block under the pointer) works
    /// in the live preview like it does over a direct attach. Active in both
    /// live-send and passive preview, mirroring the click/wheel forwards.
    /// Deduped per mapped pane cell: crossterm can re-report a cell, and one
    /// report per cell crossed is what a real terminal delivers anyway.
    /// Unlike `forward_mouse_to_preview` this never consumes the event; the
    /// caller still runs aoe's own hover handling (sidebar, dialogs) for the
    /// same motion. Returns true when a report was sent.
    pub fn forward_hover_to_preview(&mut self, col: u16, row: u16) -> bool {
        if self.has_non_live_send_overlay() || !self.hit_preview(col, row) {
            // Off-preview (or covered by a modal): drop the dedup cell so
            // re-entering the preview on the same cell reports again.
            self.hover_forward_cell = None;
            return false;
        }
        let Some(cursor) = self
            .preview_capture_worker
            .as_ref()
            .and_then(|w| w.current_cursor())
        else {
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
        // Settings is a full-screen takeover with its own scrollable
        // fields panel; the wheel drives it regardless of where the
        // (now-hidden) list/preview rects were. Checked before the
        // diff/live/dialog routing below, which would otherwise swallow
        // the wheel via `has_dialog()`.
        if let Some(view) = &mut self.settings_view {
            return view.handle_wheel_scroll(true);
        }
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
        // Full-screen (alternate-screen) app: send the wheel to the app
        // instead of scrolling the (irrelevant) normal-buffer capture. Fires
        // in both live-send and passive preview.
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
        if self.flat_items.is_empty() {
            return None;
        }
        // The list and the pinned shelf render in two separate rects, each with
        // its own scroll window, so hit-testing mirrors the render split. The
        // shelf holds the `flat_items` suffix `[list_len..]`; the list holds
        // `[..list_len]`. Both recompute the same scroll math the renderer used
        // (identical `calculate_scroll` inputs), so a click resolves to exactly
        // the row drawn under the pointer.
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
            // The synthetic Trash / Archived section headers are `Item::Group`
            // rows but they aren't user groups: a generic "Rename Group /
            // Delete Group" menu is meaningless on them. Route them to the
            // dedicated bulk menus instead (Empty Trash / Restore All /
            // collapse), keyed off the sentinel path and the section's current
            // collapsed state so the toggle label reads correctly. Only the
            // exact top-level headers qualify; project sub-folders nested under
            // Archived (project mode) keep the normal group handling, since
            // their bulk actions would act on the whole section, not the folder.
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
            // A real project header in project view gets the pin menu; the
            // cursor was just moved onto this row, so `project_group_at_cursor`
            // reflects it. Manual/synthetic group rows keep Rename/Delete.
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
                // ACP-capable. Serve builds only; the swap runs through
                // the daemon.
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
                // Same cursor-on-the-clicked-row guarantee as ToggleArchive:
                // snoozing an active row opens the duration picker, unsnoozing
                // wakes it immediately.
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
            // The right-click already moved the cursor onto the row, so reuse
            // the "new from selection" path: a session row prefills its own repo
            // path and group, a group/project row borrows a member's path, the
            // same way `'N'` does.
            ContextMenuAction::NewFromSelection => self.open_new_from_selection(),
            ContextMenuAction::Fork => self.open_fork_from_selection(),
            ContextMenuAction::SwitchView => self.prompt_switch_view_for_selected(),
            ContextMenuAction::OpenSortPicker => self.show_sort_picker(),
            ContextMenuAction::OpenGroupPicker => self.show_group_picker(),
            ContextMenuAction::TogglePin => {
                // The right-click already moved the cursor onto the project
                // header, so the toggle acts on the same project the menu was
                // opened for.
                self.toggle_project_pin_at_cursor();
            }
            // The three section-header actions read which synthetic section the
            // cursor is on (the right-click moved it onto the header). Empty
            // Trash routes through a confirm; the rest act immediately.
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

    /// Which synthetic section the cursor's `Item::Group` header belongs to, if
    /// any. Used by the section context-menu actions, which the right-click has
    /// already parked the cursor on top of.
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
        // Earned-tip signal: opening `n` with a row/group selected is exactly
        // the situation where `N` (new-from-selection) would have helped, so
        // count it. Once it crosses the threshold the "new from selection" tip
        // earns its way into the badge/list and a one-time pop (#2262).
        if self.selected_session.is_some() || self.selected_group.is_some() {
            self.record_new_session_with_selection();
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

    /// Open the tips overlay (the browsable list from `crate::tips`). Shared by
    /// the command palette, the `?` help screen, and the tips badge so they
    /// can't drift. Always opens, even with no eligible tips, so an explicit
    /// "Show tips" gives feedback (an empty state) rather than silently doing
    /// nothing.
    pub(super) fn open_tips_dialog(&mut self) {
        let config = load_config().ok().flatten().unwrap_or_default();
        let signals = crate::tips::TipSignals {
            new_session_with_selection_count: config.app_state.new_session_with_selection_count,
            used_new_from_selection: config.app_state.used_new_from_selection,
        };
        let eligible = crate::tips::eligible(crate::tips::TipSurface::Tui, &signals);
        self.tips_dialog = Some(TipsDialog::new(
            eligible,
            config.app_state.tips_seen.clone(),
            !config.session.show_tips,
            self.strict_hotkeys,
        ));
    }

    /// Persist what the tips overlay reported on close: merge newly-seen ids
    /// into `tips_seen` and apply a "don't show tips" toggle if the user
    /// flipped it. Merging (rather than overwriting) preserves seen ids for
    /// tips that aren't currently eligible.
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

    /// Record that the user has used `N` (new-from-selection). They've found the
    /// feature, so the tip teaching it is suppressed from now on; also cancel a
    /// queued pop and refresh the badge. Writes once (idempotent).
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

    /// After the new-session dialog closes, queue an earned tip to pop gently
    /// if one just became eligible and tips aren't disabled. Drained on the
    /// next keystroke by `drain_pending_tip_pop`, so it never interrupts an
    /// in-flight action.
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
        };
        self.pending_tip_pop = crate::tips::next_earned_pop(
            crate::tips::TipSurface::Tui,
            &config.app_state.tips_seen,
            &signals,
        );
    }

    /// Open the queued earned tip as a small one-tip overlay, if any. Called
    /// when the home view is idle (no other overlay) so the pop never
    /// interrupts an action. Returns true if a pop was opened, so the caller
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
        if let Some(id) = self.selected_session.clone() {
            let Some(inst) = self.get_instance(&id) else {
                return;
            };
            if matches!(inst.status, Status::Deleting | Status::Creating) {
                return;
            }
            // Rename is anchored to the selected session, so the dialog
            // must open against that session's profile, not the
            // view-level active/config profile (which can differ in
            // all-profiles mode).
            let current_profile = inst.source_profile.clone();
            let title = inst.title.clone();
            let group_path = inst.group_path.clone();
            // Capture branch context up front; a tied aoe-managed worktree
            // can opt to rename the branch alongside the directory.
            let branch_ctx = inst
                .worktree_info
                .as_ref()
                .map(|w| (w.branch.clone(), w.main_repo_path.clone()));

            let profiles = list_profiles().unwrap_or_else(|_| vec![current_profile.clone()]);
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
            // Duplicate-name validation is per-profile (rename_selected_group
            // checks only the target profile's tree), so the dialog's existing
            // names must be scoped to this group's profile too. Spanning all
            // profiles would falsely block renaming to a name that only
            // collides with a same-named group in a different profile.
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

                // Trash-first: when session.delete_to_trash is enabled (the
                // default) and the session is not already trashed, move it to
                // the trash instead of opening the permanent-delete dialog. A
                // session already in the Trash section falls through so `D`
                // there deletes it permanently. See #2489.
                let already_trashed = inst.is_trashed();
                // Resolve the policy from the SELECTED session's profile, not
                // the active filter: in all-profiles view `config_profile()`
                // can differ from the row's `source_profile`, which would
                // trash/purge under the wrong profile's retention policy.
                // See #2489.
                let session_cfg =
                    crate::session::resolve_config_or_warn(&inst.source_profile).session;
                let delete_to_trash = session_cfg.delete_to_trash;
                if delete_to_trash && !already_trashed {
                    let sid = session_id.clone();
                    // When session.confirm_delete is on, guard the trash with a
                    // confirmation dialog instead of trashing on the keystroke.
                    // The accept path (dispatch_confirm_submit "trash_session")
                    // runs the same trash_session_by_id as the instant path.
                    if session_cfg.confirm_delete {
                        let message = format!(
                            "Move '{}' to the trash? It can be restored from the Trash section.",
                            inst.title
                        );
                        self.pending_trash_session = Some(sid);
                        self.confirm_dialog = Some(ConfirmDialog::new(
                            "Confirm Delete",
                            &message,
                            "trash_session",
                        ));
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
            if self.group_by == GroupByMode::Project {
                let hint = if self.strict_hotkeys {
                    "Project groups are automatic. Press Ctrl+G and pick Manual to manage groups."
                } else {
                    "Project groups are automatic. Press 'g' and pick Manual to manage groups."
                };
                self.info_dialog = Some(InfoDialog::new("Cannot Modify Project Groups", hint));
                return;
            }
            // Scope the count to the selected group's profile: two groups in
            // different profiles can share a path, and counting by path alone
            // would pop the "delete N sessions" options dialog for an empty
            // group whose same-named twin in another profile still has rows.
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

    /// A double-click on the preview pane opens/attaches the previewed session,
    /// producing the SAME `Action` a sidebar double-click (or `Enter`) would, so
    /// the two gestures match. Mirrors `handle_click`'s double-click detection
    /// but keyed to the preview rect via its own `last_preview_click`, and gated
    /// to a plain (no-Shift) left press over the preview with no overlay on top.
    /// The previewed pane is always `selected_session`, so it activates the
    /// right row without touching `cursor`. Returns the activation `Action` on
    /// the second qualifying press on the SAME cell within
    /// `DOUBLE_CLICK_THRESHOLD`, else `None` (a single press, whose timing it
    /// records; the caller still forwards that press to a mouse-tracking
    /// agent). Shift falls through so aoe's own preview selection runs, matching
    /// the mouse-forward gate.
    pub fn preview_double_click_action(
        &mut self,
        kind: crossterm::event::MouseEventKind,
        modifiers: crossterm::event::KeyModifiers,
        col: u16,
        row: u16,
    ) -> Option<Action> {
        self.preview_double_click_action_at(std::time::Instant::now(), kind, modifiers, col, row)
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
        // Match the SAME cell, not just the row: the sidebar can key by row
        // because a row identifies an item, but a preview row is just a line of
        // text, so two unrelated presses on the same line (different columns)
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

        // Footer-toolbar hover: the hovered button's shortcut drives the
        // inverted-chip highlight on the next render. Recomputed against the
        // current button rects so it clears the moment the pointer leaves a
        // button.
        let prev_footer_hover = self.footer_hover;
        self.footer_hover = self
            .footer_buttons
            .iter()
            .find(|(rect, _)| rect.contains(Position::from((col, row))))
            .map(|(_, key)| *key);
        let footer_changed = prev_footer_hover != self.footer_hover;

        // Hover is live over both the scrolling list and the pinned shelf, so
        // a shelf row (Trash / Archived) highlights under the pointer the same
        // way a list row does. `resolve_row_to_index` maps either region.
        let over_sidebar = self.list_inner_area.contains(Position::from((col, row)))
            || self.shelf_inner_area.contains(Position::from((col, row)));
        let new_pos = if over_sidebar { Some((col, row)) } else { None };
        let prev_idx = self.hovered_index();
        self.mouse_pos = new_pos;
        let new_idx = self.hovered_index();

        // Footer tips badge: highlight on hover like a session row. Gated to no
        // overlay being open (the badge isn't clickable then), matching
        // `handle_tips_badge_click`.
        let badge_hover = !self.has_non_live_send_overlay()
            && self
                .tips_badge_rect
                .is_some_and(|r| r.contains(Position::from((col, row))));
        let badge_changed = badge_hover != self.tips_badge_hovered;
        self.tips_badge_hovered = badge_hover;

        overlay_changed || footer_changed || badge_changed || prev_idx != new_idx
    }

    /// Route a mouse-wheel-down at (col, row); see handle_scroll_up.
    pub fn handle_scroll_down(&mut self, col: u16, row: u16) -> bool {
        const STEP: u16 = 3;
        // Settings takeover owns the wheel; see handle_scroll_up.
        if let Some(view) = &mut self.settings_view {
            return view.handle_wheel_scroll(false);
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
        // the user sees. Tool view → the named tool's paired pane, so
        // live-send can drive lazygit/yazi/etc. the same way.
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
        // Ctrl+C reaching this point is forwarded to the agent rather than
        // quitting aoe (the app-level global handler defers to live-send via
        // `is_live_send_capturing`). Flash the footer so the user learns the
        // keystroke landed on the agent; re-armed on every press (#2894).
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

    /// Exit live-send before an activation hands the terminal to a tmux
    /// attach. The double-click (and Enter) activation path resolves
    /// through `default_attach_mode`, so when `click_action = LiveSend`
    /// the *first* click of a double-click already entered live-send for
    /// the row; the second click then resolves to a tmux attach
    /// (`default_attach_mode = Tmux`). Without this teardown the
    /// just-spawned worker keeps dispatching against a pane we're
    /// leaving, the attach inherits the preview-pinned window size
    /// instead of growing to the client, and detaching drops the user
    /// back into live mode rather than the home list (#2290). No-op when
    /// not live-sending.
    fn exit_live_send_before_attach(&mut self) {
        if let Some(state) = self.live_send.clone() {
            self.exit_live_send_and_restore_sizing(&state);
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
        // The leader menu is live-mode-only: drop any half-entered chord so
        // the home view is never left armed. The sidebar collapse is now a
        // general, persisted home-view state (the collapsed strip stays
        // clickable here), so exiting live mode deliberately leaves it as
        // the user set it rather than force-revealing the list.
        self.live_send_pending_leader = false;
        // The Ctrl+C footer flash is live-mode-only; drop any pending window
        // so it can't linger onto the home-view footer after exit (#2894).
        self.live_send_ctrl_c_flash_until = None;
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
        let current_name = match &state.target {
            live_send::LiveSendTarget::Agent => {
                crate::tmux::Session::generate_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::Terminal => {
                crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title)
            }
            live_send::LiveSendTarget::ContainerTerminal => {
                crate::tmux::ContainerTerminalSession::generate_name(&inst.id, &inst.title)
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

    /// Open the permission-response dialog for the currently-selected
    /// session, letting the user answer a permission prompt they can see
    /// in the pane without attaching. Unlike `open_send_message_dialog`,
    /// this has no `Status::Waiting` gate: the user has already visually
    /// confirmed the prompt is showing, and AoE never parses pane content
    /// to verify it. Silently no-ops when there's no valid session
    /// selected or it's mid create/delete; shows an info dialog when the
    /// selected session's agent has no mapped keystroke sequences yet.
    fn open_permission_response_dialog(&mut self) {
        let Some(id) = self.selected_session.clone() else {
            return;
        };
        let Some(inst) = self.get_instance(&id) else {
            return;
        };
        if matches!(inst.status, Status::Creating | Status::Deleting) {
            return;
        }
        if inst.is_structured() {
            return;
        }
        let title = inst.title.clone();
        let tool = inst.tool.clone();
        let supported = crate::agents::get_agent(&tool)
            .and_then(|a| a.permission_response)
            .is_some();
        if !supported {
            self.info_dialog = Some(InfoDialog::new(
                "Not Supported",
                &format!("{} doesn't support quick permission responses yet.", tool),
            ));
            return;
        }
        self.pending_permission_response_session = Some(id);
        self.permission_response_dialog =
            Some(crate::tui::dialogs::PermissionResponseDialog::new(&title));
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

    /// Rebuild `flat_items` and re-score any committed `search_matches`
    /// against the new indices. Every mutation site that touches
    /// `self.flat_items` must go through this helper or `search_matches`
    /// retains stale indices into the old list, and `n`/`N` cycles jump
    /// to wrong sessions (row highlights follow the stale indices too).
    /// See #2676.
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
        }
    }

    /// `hover_forward_bytes` only fires for a full-screen app in any-event
    /// tracking (1003), and then emits the no-button motion report (SGR 35 /
    /// the X10 equivalent) in the app's encoding. Everything else, including
    /// a button-tracking (1000/1002) app that never expects bare motion,
    /// gets `None`.
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

    /// The fix for #2407: a full-screen pane with no mouse tracking must
    /// forward `PageUp`/`PageDown`, NOT arrow keys (which an app reads as
    /// cursor / input-history navigation, not scroll) and NOT raw mouse
    /// bytes. Asserting the key variant guards against a regression to
    /// either that the preview-offset behavioral test can't catch.
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
        // Users firing 'm' from Terminal view should see the dialog
        // title (and the live-mode banner) call out the target pane so
        // they don't accidentally send agent prompts into a shell (or
        // vice versa). Both the compose dialog and the status-bar
        // banner route through the same helper so the label can't drift.
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
        // `isolate_home` restores HOME/XDG on Drop (the old bare `set_var`
        // leaked the deleted tempdir into later tests) and holds the
        // process-global env lock for the guard's lifetime.
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
