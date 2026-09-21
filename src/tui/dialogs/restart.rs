//! Restart session dialog: pick profile + AI engine before respawning.
//!
//! Profile-on-restart means a heavy respawn that re-applies the new
//! profile's env (CLAUDE_CONFIG_DIR, API keys, MCP servers). Picking a
//! profile auto-populates the tool from `config.session.default_tool`,
//! mirroring `NewSessionDialog::reload_config_defaults`. A manual tool
//! override does not snap the profile, so users can keep the profile
//! and swap only the engine.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::Input;

use super::DialogResult;
use crate::session::config::profile_config::resolve_config_or_warn;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::components::{
    handle_tool_config_key, profile_cycler_spans, render_tool_config_overlay, tool_cycler_spans,
    tool_row_suffix_spans, ToolConfigOutcome,
};
use crate::tui::styles::Theme;

/// Data returned when the restart dialog is submitted.
#[derive(Debug, Clone)]
pub struct RestartData {
    /// New profile (None means keep current).
    pub profile: Option<String>,
    /// New tool (None means keep current).
    pub tool: Option<String>,
    /// New extra args (None means keep current).
    pub extra_args: Option<String>,
    /// New command override (None means keep current).
    pub command_override: Option<String>,
}

pub struct RestartDialog {
    current_title: String,
    current_profile: String,
    current_tool: String,
    /// The instance's current launch command and extra args, used to decide
    /// whether the submitted values actually changed.
    current_command_override: String,
    current_extra_args: String,
    available_profiles: Vec<String>,
    available_tools: Vec<String>,
    profile_index: usize,
    tool_index: usize,
    /// 0 = profile, 1 = tool.
    focused_field: usize,
    /// Editable command override, shown in the tool-config overlay.
    command_override: Input,
    /// Editable extra args, shown in the tool-config overlay.
    extra_args: Input,
    /// True while the Ctrl+P tool-config overlay is open.
    tool_config_mode: bool,
    /// 0 = command override, 1 = extra args.
    tool_config_focused_field: usize,
    /// Per-field hit rects for the tool-config overlay, so a click can focus
    /// the Command/Extra-Args field directly (parity with the New dialog).
    tool_config_rects: Vec<(usize, Rect)>,
    profile_selector_area: Rect,
    tool_selector_area: Rect,
    /// Which selector row the mouse is over, for the hover highlight.
    /// Visual only; never moves keyboard `focused_field`.
    hover: HoverState,
}

impl RestartDialog {
    pub fn new(
        current_title: &str,
        current_profile: &str,
        current_tool: &str,
        current_command_override: &str,
        current_extra_args: &str,
        available_profiles: Vec<String>,
        available_tools: Vec<String>,
    ) -> Self {
        let profile_index = available_profiles
            .iter()
            .position(|p| p == current_profile)
            .unwrap_or(0);
        let tool_index = available_tools
            .iter()
            .position(|t| t == current_tool)
            .unwrap_or(0);

        Self {
            current_title: current_title.to_string(),
            current_profile: current_profile.to_string(),
            current_tool: current_tool.to_string(),
            current_command_override: current_command_override.to_string(),
            current_extra_args: current_extra_args.to_string(),
            available_profiles,
            available_tools,
            profile_index,
            tool_index,
            focused_field: 0,
            command_override: Input::new(current_command_override.to_string()),
            extra_args: Input::new(current_extra_args.to_string()),
            tool_config_mode: false,
            tool_config_focused_field: 0,
            tool_config_rects: Vec::new(),
            profile_selector_area: Rect::default(),
            tool_selector_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<RestartData>> {
        let pos = ratatui::layout::Position::from((col, row));
        // While the tool-config overlay is up, a click on a field focuses it;
        // any other click is swallowed so a stray click on the (now-hidden)
        // selectors underneath can't cycle them.
        if self.tool_config_mode {
            if let Some(hit) = self
                .tool_config_rects
                .iter()
                .find(|(_, rect)| rect.contains(pos))
                .map(|(f, _)| *f)
            {
                self.tool_config_focused_field = hit;
            }
            return Some(DialogResult::Continue);
        }
        if self.profile_selector_area.contains(pos) {
            self.focused_field = 0;
            if !self.available_profiles.is_empty() {
                self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
                // Mirror keyboard cycling: when the profile changes,
                // re-resolve the tool default so the picker updates too.
                self.sync_tool_from_profile();
            }
            return Some(DialogResult::Continue);
        }
        if self.tool_selector_area.contains(pos) {
            self.focused_field = 1;
            if !self.available_tools.is_empty() {
                self.tool_index = (self.tool_index + 1) % self.available_tools.len();
                self.reload_tool_config();
            }
            return Some(DialogResult::Continue);
        }
        None
    }

    /// Highlight the selector row under the cursor without moving the
    /// focused field. Click commits via `handle_click`; see
    /// `ConfirmDialog::handle_hover` for the rationale (mouse drift
    /// between the user reading the dialog and hitting a keystroke must
    /// not silently shift which field that key targets). Returns `true`
    /// when the highlighted row changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        // The overlay covers the selectors; don't highlight rows beneath it.
        if self.tool_config_mode {
            return false;
        }
        self.hover.update(
            col,
            row,
            &[self.profile_selector_area, self.tool_selector_area],
        )
    }

    /// Re-resolve the default tool for the currently selected profile
    /// and snap `tool_index` accordingly, matching the keyboard's
    /// "cycle profile -> auto-pick its default_tool" behavior.
    fn sync_tool_from_profile(&mut self) {
        let Some(profile) = self.selected_profile().map(String::from) else {
            return;
        };
        let cfg = resolve_config_or_warn(&profile);
        if let Some(default_tool) = cfg.session.default_tool.as_ref() {
            if let Some(idx) = self.available_tools.iter().position(|t| t == default_tool) {
                self.tool_index = idx;
            }
        }
        self.reload_tool_config();
    }

    /// Returns the selected profile, or `None` if no profiles are
    /// available. The dialog refuses to submit in the `None` case; the
    /// no-profile state is only reachable via a bad config, but the
    /// panic-free path is cheap.
    fn selected_profile(&self) -> Option<&str> {
        self.available_profiles
            .get(self.profile_index)
            .map(String::as_str)
    }

    fn selected_tool(&self) -> Option<&str> {
        self.available_tools
            .get(self.tool_index)
            .map(String::as_str)
    }

    /// Profile change snaps tool to the profile's `default_tool` if that tool
    /// exists in `available_tools`; otherwise leaves tool_index where it was.
    /// Mirrors NewSessionDialog::reload_config_defaults so the behavior of
    /// "picking a profile pre-populates the AI engine" matches across the
    /// New / Rename / Restart modals.
    fn reload_tool_from_profile(&mut self) {
        let Some(profile) = self.selected_profile().map(str::to_string) else {
            return;
        };
        let config = resolve_config_or_warn(&profile);
        if let Some(ref default_tool) = config.session.default_tool {
            if let Some(idx) = self.available_tools.iter().position(|t| t == default_tool) {
                self.tool_index = idx;
            }
        }
        self.reload_tool_config();
    }

    /// Re-seed the command override and extra args inputs from the selected
    /// profile's config for the selected tool. Mirrors
    /// `NewSessionDialog::reload_tool_config` so swapping the engine in the
    /// restart modal picks up that tool's configured defaults.
    fn reload_tool_config(&mut self) {
        let Some(profile) = self.selected_profile().map(str::to_string) else {
            return;
        };
        let tool = self
            .selected_tool()
            .or_else(|| self.available_tools.first().map(String::as_str))
            .unwrap_or("claude")
            .to_string();

        // Round-trip guard: if the user cycled away and landed back on the
        // session's original profile + tool, restore the instance's live
        // command/args rather than the profile defaults. Otherwise an A->B->A
        // bounce would silently replace the existing overrides with defaults
        // and submit would treat that as a real edit (see #2041 review).
        if profile == self.current_profile && tool == self.current_tool {
            self.extra_args = Input::new(self.current_extra_args.clone());
            self.command_override = Input::new(self.current_command_override.clone());
            return;
        }

        let config = resolve_config_or_warn(&profile);
        self.extra_args = Input::new(
            config
                .session
                .agent_extra_args
                .get(&tool)
                .cloned()
                .unwrap_or_default(),
        );
        self.command_override = Input::new(config.session.resolve_tool_command(&tool));
    }

    fn next_field(&mut self) {
        self.focused_field = (self.focused_field + 1) % 2;
    }

    fn prev_field(&mut self) {
        self.focused_field = if self.focused_field == 0 { 1 } else { 0 };
    }

    fn is_profile_field(&self) -> bool {
        self.focused_field == 0
    }

    fn is_tool_field(&self) -> bool {
        self.focused_field == 1
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<RestartData> {
        if self.tool_config_mode {
            return self.handle_tool_config_key(key);
        }

        // Ctrl+P opens the tool-config overlay (command override + extra
        // args), but only when the tool field is focused, mirroring the
        // new-session dialog's "(Ctrl + P to configure)" affordance.
        if key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.is_tool_field()
        {
            self.tool_config_mode = true;
            self.tool_config_focused_field = 0;
            return DialogResult::Continue;
        }

        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => {
                let Some(new_profile) = self.selected_profile().map(str::to_string) else {
                    // No profiles available; refuse submit. Caller decides
                    // whether to keep the dialog open or close it.
                    return DialogResult::Continue;
                };
                let new_tool = self.selected_tool().map(str::to_string);
                let profile = if new_profile == self.current_profile {
                    None
                } else {
                    Some(new_profile)
                };
                let tool = match new_tool {
                    Some(t) if t == self.current_tool => None,
                    other => other,
                };
                let extra_args = {
                    let value = self.extra_args.value().trim().to_string();
                    if value == self.current_extra_args.trim() {
                        None
                    } else {
                        Some(value)
                    }
                };
                let command_override = {
                    let value = self.command_override.value().trim().to_string();
                    if value == self.current_command_override.trim() {
                        None
                    } else {
                        Some(value)
                    }
                };
                DialogResult::Submit(RestartData {
                    profile,
                    tool,
                    extra_args,
                    command_override,
                })
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_field();
                } else {
                    self.next_field();
                }
                DialogResult::Continue
            }
            KeyCode::Down => {
                self.next_field();
                DialogResult::Continue
            }
            KeyCode::Up => {
                self.prev_field();
                DialogResult::Continue
            }
            KeyCode::Left if self.is_profile_field() => {
                if self.available_profiles.is_empty() {
                    return DialogResult::Continue;
                }
                self.profile_index = if self.profile_index == 0 {
                    self.available_profiles.len() - 1
                } else {
                    self.profile_index - 1
                };
                self.reload_tool_from_profile();
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char(' ') if self.is_profile_field() => {
                if self.available_profiles.is_empty() {
                    return DialogResult::Continue;
                }
                self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
                self.reload_tool_from_profile();
                DialogResult::Continue
            }
            KeyCode::Left if self.is_tool_field() => {
                if self.available_tools.is_empty() {
                    return DialogResult::Continue;
                }
                self.tool_index = if self.tool_index == 0 {
                    self.available_tools.len() - 1
                } else {
                    self.tool_index - 1
                };
                self.reload_tool_config();
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char(' ') if self.is_tool_field() => {
                if self.available_tools.is_empty() {
                    return DialogResult::Continue;
                }
                self.tool_index = (self.tool_index + 1) % self.available_tools.len();
                self.reload_tool_config();
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// Handle key events while the tool-config overlay is open, delegating to
    /// the shared component. Enter/Esc close the overlay (they never submit or
    /// cancel the parent dialog).
    fn handle_tool_config_key(&mut self, key: KeyEvent) -> DialogResult<RestartData> {
        match handle_tool_config_key(
            key,
            &mut self.command_override,
            &mut self.extra_args,
            &mut self.tool_config_focused_field,
        ) {
            ToolConfigOutcome::Close => self.tool_config_mode = false,
            ToolConfigOutcome::Continue => {}
        }
        DialogResult::Continue
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Wide enough that the Tool row's "(configured)  Ctrl+P: edit" suffix
        // isn't clipped (the cycler + suffix run past the old 54-col width).
        let block = super::dialog_block(" Restart Session ", theme);
        let (_, inner) = super::render_dialog_frame(frame, area, 64, 14, block);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // Title row
                Constraint::Length(1), // Current profile
                Constraint::Length(1), // Current tool
                Constraint::Length(1), // Spacer
                Constraint::Length(1), // Profile selector
                Constraint::Length(1), // Tool selector
                Constraint::Length(1), // Spacer
                Constraint::Min(1),    // Hint
            ])
            .split(inner);

        let title_line = Line::from(vec![
            Span::styled("Session: ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.current_title, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(title_line), chunks[0]);

        let current_profile_line = Line::from(vec![
            Span::styled("Current profile: ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.current_profile, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(current_profile_line), chunks[1]);

        let current_tool_line = Line::from(vec![
            Span::styled("Current tool:    ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.current_tool, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(current_tool_line), chunks[2]);

        self.render_profile_selector(frame, chunks[4], theme);
        self.profile_selector_area = chunks[4];
        self.render_tool_selector(frame, chunks[5], theme);
        self.tool_selector_area = chunks[5];
        self.render_hints(frame, chunks[7], theme);

        if let Some(rect) = self
            .hover
            .current_in(&[self.profile_selector_area, self.tool_selector_area])
        {
            paint_hover_bg(frame, rect, theme.selection);
        }

        if self.tool_config_mode {
            let selected_tool = self
                .available_tools
                .get(self.tool_index)
                .or_else(|| self.available_tools.first())
                .map(String::as_str)
                .unwrap_or("claude");
            self.tool_config_rects = render_tool_config_overlay(
                frame,
                area,
                selected_tool,
                &self.command_override,
                &self.extra_args,
                self.tool_config_focused_field,
                theme,
            );
        }
    }

    /// Profile picker, rendered via the shared `profile_cycler_spans` so the
    /// New and Restart modals stay visually identical.
    fn render_profile_selector(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let value = self
            .available_profiles
            .get(self.profile_index)
            .map(String::as_str)
            .unwrap_or("(none)");
        let spans = profile_cycler_spans(
            "Profile:",
            value,
            self.available_profiles.len(),
            self.is_profile_field(),
            theme,
        );
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    /// AI-engine picker, rendered via the shared `tool_cycler_spans` so the
    /// label reads "Tool:" and the cycler matches the New dialog exactly. The
    /// Restart dialog appends the same "(configured)" summary and Ctrl+P hint
    /// the New dialog does, so the tool-config overlay is discoverable inline.
    fn render_tool_selector(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let value = self
            .available_tools
            .get(self.tool_index)
            .map(String::as_str)
            .unwrap_or("(none)");
        let mut spans = tool_cycler_spans(
            "Tool:",
            value,
            self.tool_index,
            self.available_tools.len(),
            self.is_tool_field(),
            theme,
        );
        let has_config =
            !self.extra_args.value().is_empty() || !self.command_override.value().is_empty();
        spans.extend(tool_row_suffix_spans(
            value,
            has_config,
            self.is_tool_field(),
            theme,
        ));
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_hints(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // Ctrl+P is surfaced inline next to the Tool row (see
        // `render_tool_selector`), so it stays out of this footer.
        let hint = Line::from(vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" switch  "),
            Span::styled("← →", Style::default().fg(theme.hint)),
            Span::raw(" cycle  "),
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" restart  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" cancel"),
        ]);
        frame.render_widget(Paragraph::new(hint), area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::{ctrl_key, key, shift_key};

    fn profiles() -> Vec<String> {
        ["default", "work", "personal"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    fn tools() -> Vec<String> {
        ["claude", "codex", "settl"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }

    /// A dialog with no pre-existing command override or extra args.
    fn dialog() -> RestartDialog {
        RestartDialog::new("S", "default", "claude", "", "", profiles(), tools())
    }

    /// A dialog seeded with live overrides on its current tool.
    fn configured() -> RestartDialog {
        RestartDialog::new(
            "S",
            "default",
            "claude",
            "claude-wrapper",
            "--foo",
            profiles(),
            tools(),
        )
    }

    /// Open the tool-config overlay, which only Ctrl+P on the tool field does.
    fn with_overlay() -> RestartDialog {
        let mut d = dialog();
        d.focused_field = 1;
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert!(d.tool_config_mode);
        d
    }

    fn submitted(result: DialogResult<RestartData>) -> RestartData {
        match result {
            DialogResult::Submit(data) => data,
            _ => panic!("expected Submit"),
        }
    }

    #[test]
    fn new_seeds_from_the_current_session() {
        let d = RestartDialog::new("My Sess", "work", "codex", "", "", profiles(), tools());
        assert_eq!((d.profile_index, d.tool_index, d.focused_field), (1, 1, 0));

        // A profile or tool that is no longer listed falls back to the first.
        let d = RestartDialog::new("S", "ghost", "ghost-tool", "", "", profiles(), tools());
        assert_eq!((d.profile_index, d.tool_index), (0, 0));

        let d = configured();
        assert_eq!(d.command_override.value(), "claude-wrapper");
        assert_eq!(d.extra_args.value(), "--foo");
        assert!(!d.tool_config_mode);
    }

    #[test]
    fn submits_only_what_changed() {
        let unchanged = submitted(dialog().handle_key(key(KeyCode::Enter)));
        assert_eq!(unchanged.profile, None);
        assert_eq!(unchanged.tool, None);
        assert_eq!(unchanged.extra_args, None);
        assert_eq!(unchanged.command_override, None);

        // Seeded overrides that were never edited also submit as None.
        let untouched = submitted(configured().handle_key(key(KeyCode::Enter)));
        assert_eq!(untouched.command_override, None);
        assert_eq!(untouched.extra_args, None);

        let mut d = dialog();
        d.handle_key(key(KeyCode::Right));
        assert_eq!(
            submitted(d.handle_key(key(KeyCode::Enter))).profile,
            Some("work".to_string())
        );

        assert!(matches!(
            dialog().handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        assert!(matches!(
            dialog().handle_key(key(KeyCode::Char('x'))),
            DialogResult::Continue
        ));

        // A pathological empty profile list must not index-panic; the dialog
        // refuses to submit and leaves the decision to the caller.
        let mut empty = RestartDialog::new("S", "default", "claude", "", "", vec![], tools());
        assert!(matches!(
            empty.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        ));
    }

    #[test]
    fn tab_and_arrows_move_focus_and_cycle_the_profile() {
        let mut d = dialog();
        for expected in [1, 0] {
            d.handle_key(key(KeyCode::Tab));
            assert_eq!(d.focused_field, expected);
        }
        for expected in [1, 0] {
            d.handle_key(shift_key(KeyCode::Tab));
            assert_eq!(d.focused_field, expected);
        }

        // Right and Space cycle forward, Left backward, both wrapping.
        for code in [KeyCode::Right, KeyCode::Char(' ')] {
            let mut d = dialog();
            for expected in [1, 2, 0] {
                d.handle_key(key(code));
                assert_eq!(d.profile_index, expected);
            }
        }
        let mut d = dialog();
        for expected in [2, 1] {
            d.handle_key(key(KeyCode::Left));
            assert_eq!(d.profile_index, expected);
        }
    }

    #[test]
    #[serial_test::serial]
    fn cycling_the_tool_leaves_the_profile_and_live_overrides_intact() {
        let mut d = dialog();
        d.focused_field = 1;
        for (code, expected) in [(KeyCode::Right, 1), (KeyCode::Left, 0), (KeyCode::Left, 2)] {
            d.handle_key(key(code));
            assert_eq!(d.tool_index, expected);
        }
        assert_eq!(d.profile_index, 0, "a tool override must not snap profile");

        let mut d = dialog();
        d.focused_field = 1;
        d.handle_key(key(KeyCode::Right));
        let data = submitted(d.handle_key(key(KeyCode::Enter)));
        assert_eq!(data.profile, None);
        assert_eq!(data.tool, Some("codex".to_string()));

        // Cycling away and back restores the session's live overrides rather
        // than the new tool's config defaults, so the round trip is a no-op.
        let mut d = configured();
        d.focused_field = 1;
        d.handle_key(key(KeyCode::Right));
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.tool_index, 0);
        assert_eq!(d.command_override.value(), "claude-wrapper");
        assert_eq!(d.extra_args.value(), "--foo");
        let data = submitted(d.handle_key(key(KeyCode::Enter)));
        assert_eq!(data.tool, None);
        assert_eq!(data.command_override, None);
        assert_eq!(data.extra_args, None);
    }

    #[test]
    fn tool_config_overlay_opens_only_from_the_tool_field() {
        let mut d = dialog();
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert!(!d.tool_config_mode, "profile field has no overlay");
        assert_eq!(with_overlay().tool_config_focused_field, 0);
    }

    #[test]
    fn tool_config_edits_both_fields_and_submits_them() {
        let mut d = with_overlay();
        d.handle_key(key(KeyCode::Char('z')));
        assert_eq!(d.command_override.value(), "z");
        for expected in [1, 0] {
            d.handle_key(key(KeyCode::Tab));
            assert_eq!(d.tool_config_focused_field, expected);
        }

        for (tabs, arg, command) in [(1, Some("-v"), None), (0, None, Some("w"))] {
            let mut d = with_overlay();
            for _ in 0..tabs {
                d.handle_key(key(KeyCode::Tab));
            }
            for c in arg.or(command).expect("one field edited").chars() {
                d.handle_key(key(KeyCode::Char(c)));
            }
            d.handle_key(key(KeyCode::Enter));
            let data = submitted(d.handle_key(key(KeyCode::Enter)));
            assert_eq!(data.extra_args.as_deref(), arg);
            assert_eq!(data.command_override.as_deref(), command);
        }

        // Esc and Enter both leave the overlay without deciding the dialog.
        for code in [KeyCode::Esc, KeyCode::Enter] {
            let mut d = with_overlay();
            assert!(matches!(d.handle_key(key(code)), DialogResult::Continue));
            assert!(!d.tool_config_mode);
        }
    }

    #[test]
    fn clicks_in_the_overlay_focus_a_field_and_a_miss_changes_nothing() {
        let mut d = with_overlay();
        // Staged manually; the real rects come from render().
        d.tool_config_rects = vec![(0, Rect::new(2, 4, 60, 1)), (1, Rect::new(2, 6, 60, 1))];
        for (row, expected) in [(6, 1), (4, 0)] {
            d.handle_click(4, row);
            assert_eq!(d.tool_config_focused_field, expected);
        }

        // A click that misses every rect is swallowed: it must not move focus
        // or cycle the selectors hidden beneath the overlay.
        let tool_index = d.tool_index;
        d.handle_click(0, 0);
        assert_eq!(d.tool_config_focused_field, 0);
        assert_eq!(d.tool_index, tool_index);
    }

    #[test]
    fn hover_highlights_a_selector_without_moving_focus() {
        let mut d = dialog();
        d.profile_selector_area = Rect::new(2, 4, 50, 1);
        d.tool_selector_area = Rect::new(2, 5, 50, 1);
        assert!(d.handle_hover(5, 5));
        assert_eq!(d.hover.current(), Some(d.tool_selector_area));
        assert_eq!(d.focused_field, 0, "hover must not move the focused field");
        assert!(d.handle_hover(99, 99));
        assert_eq!(d.hover.current(), None);
    }

    #[test]
    fn deprecated_tool_badge_survives_a_constrained_configured_row() {
        // Real geometry: 64 columns leaves a 60-column inner row. With three
        // tools, a command override and extra args, the lifecycle suffix has
        // to survive the trailing configuration metadata.
        use ratatui::{backend::TestBackend, Terminal};

        for (tool, deprecated) in [("gemini", true), ("claude", false)] {
            let mut dialog = RestartDialog::new(
                "fix bug",
                "default",
                tool,
                "custom-wrapper",
                "--model long --verbose",
                vec!["default".to_string()],
                ["claude", "codex", "gemini"]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            );
            dialog.focused_field = 1;
            let mut terminal = Terminal::new(TestBackend::new(64, 14)).expect("terminal");
            let theme = crate::tui::styles::Theme::default();
            terminal
                .draw(|frame| dialog.render(frame, frame.area(), &theme))
                .expect("render");
            let content = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert_eq!(
                content.contains("⚠ deprecated"),
                deprecated,
                "{tool}: {content}"
            );
            assert!(content.contains("(configured)"), "{tool}: {content}");
            assert!(content.contains("Ctrl+P"), "{tool}: {content}");
            if deprecated {
                assert!(content.contains("(configured) Ctrl+P"), "{content}");
            }
        }
    }
}
