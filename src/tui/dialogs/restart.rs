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

use super::DialogResult;
use crate::session::profile_config::resolve_config_or_warn;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::components::{profile_cycler_spans, tool_cycler_spans};
use crate::tui::styles::Theme;

/// Data returned when the restart dialog is submitted.
#[derive(Debug, Clone)]
pub struct RestartData {
    /// New profile (None means keep current).
    pub profile: Option<String>,
    /// New tool (None means keep current).
    pub tool: Option<String>,
}

pub struct RestartDialog {
    current_title: String,
    current_profile: String,
    current_tool: String,
    available_profiles: Vec<String>,
    available_tools: Vec<String>,
    profile_index: usize,
    tool_index: usize,
    /// 0 = profile, 1 = tool.
    focused_field: usize,
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
            available_profiles,
            available_tools,
            profile_index,
            tool_index,
            focused_field: 0,
            profile_selector_area: Rect::default(),
            tool_selector_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<RestartData>> {
        let pos = ratatui::layout::Position::from((col, row));
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
                DialogResult::Submit(RestartData { profile, tool })
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
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char(' ') if self.is_tool_field() => {
                if self.available_tools.is_empty() {
                    return DialogResult::Continue;
                }
                self.tool_index = (self.tool_index + 1) % self.available_tools.len();
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_area = super::centered_rect(area, 54, 14);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Restart Session ")
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

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
    /// label reads "Tool:" and the cycler matches the New dialog exactly.
    fn render_tool_selector(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let value = self
            .available_tools
            .get(self.tool_index)
            .map(String::as_str)
            .unwrap_or("(none)");
        let spans = tool_cycler_spans(
            "Tool:",
            value,
            self.tool_index,
            self.available_tools.len(),
            self.is_tool_field(),
            theme,
        );
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_hints(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
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
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn profiles() -> Vec<String> {
        vec![
            "default".to_string(),
            "work".to_string(),
            "personal".to_string(),
        ]
    }

    fn tools() -> Vec<String> {
        vec![
            "claude".to_string(),
            "codex".to_string(),
            "settl".to_string(),
        ]
    }

    #[test]
    fn test_new_seeds_indices_from_current() {
        let d = RestartDialog::new("My Sess", "work", "codex", profiles(), tools());
        assert_eq!(d.profile_index, 1);
        assert_eq!(d.tool_index, 1);
        assert_eq!(d.focused_field, 0);
    }

    #[test]
    fn test_new_falls_back_when_current_not_in_list() {
        let d = RestartDialog::new("S", "ghost", "ghost-tool", profiles(), tools());
        assert_eq!(d.profile_index, 0);
        assert_eq!(d.tool_index, 0);
    }

    #[test]
    fn test_esc_cancels() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        assert!(matches!(
            d.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn test_enter_with_no_changes_returns_none_for_both() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        match d.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(data) => {
                assert_eq!(data.profile, None);
                assert_eq!(data.tool, None);
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_tab_cycles_focus() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        assert_eq!(d.focused_field, 0);
        d.handle_key(key(KeyCode::Tab));
        assert_eq!(d.focused_field, 1);
        d.handle_key(key(KeyCode::Tab));
        assert_eq!(d.focused_field, 0);
    }

    #[test]
    fn test_shift_tab_cycles_focus_backwards() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.handle_key(shift_key(KeyCode::Tab));
        assert_eq!(d.focused_field, 1);
        d.handle_key(shift_key(KeyCode::Tab));
        assert_eq!(d.focused_field, 0);
    }

    #[test]
    fn test_right_cycles_profile_when_profile_focused() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.profile_index, 1);
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.profile_index, 2);
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.profile_index, 0); // wrap
    }

    #[test]
    fn test_left_cycles_profile_backwards_when_profile_focused() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.profile_index, 2); // wrap to end
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.profile_index, 1);
    }

    #[test]
    fn test_space_also_cycles_profile_forward() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(d.profile_index, 1);
    }

    #[test]
    fn test_arrows_cycle_tool_when_tool_focused() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.focused_field = 1;
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.tool_index, 1);
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.tool_index, 0);
        d.handle_key(key(KeyCode::Left));
        assert_eq!(d.tool_index, 2); // wrap
    }

    #[test]
    fn test_profile_change_submits_some() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.handle_key(key(KeyCode::Right)); // profile -> work
        match d.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(data) => {
                assert_eq!(data.profile, Some("work".to_string()));
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_tool_only_change_submits_tool_some_profile_none() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.focused_field = 1;
        d.handle_key(key(KeyCode::Right)); // tool -> codex
        match d.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(data) => {
                assert_eq!(data.profile, None);
                assert_eq!(data.tool, Some("codex".to_string()));
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_tool_override_does_not_snap_profile() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.focused_field = 1;
        d.handle_key(key(KeyCode::Right));
        assert_eq!(d.profile_index, 0); // profile unchanged
    }

    #[test]
    fn test_unknown_key_is_continue() {
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        assert!(matches!(
            d.handle_key(key(KeyCode::Char('x'))),
            DialogResult::Continue
        ));
    }

    #[test]
    fn hover_highlights_selector_without_moving_focus() {
        // Stage selector rects manually; the real ones come from render().
        let mut d = RestartDialog::new("S", "default", "claude", profiles(), tools());
        d.profile_selector_area = Rect::new(2, 4, 50, 1);
        d.tool_selector_area = Rect::new(2, 5, 50, 1);
        assert_eq!(d.focused_field, 0);

        // Over the tool row: highlight it, focus unchanged.
        assert!(d.handle_hover(5, 5));
        assert_eq!(d.hover.current(), Some(d.tool_selector_area));
        assert_eq!(d.focused_field, 0, "hover must not move the focused field");

        // Off both rows clears the highlight.
        assert!(d.handle_hover(99, 99));
        assert_eq!(d.hover.current(), None);
    }

    #[test]
    fn test_enter_with_empty_profiles_does_not_panic() {
        // Pathological config (empty profiles list); Enter must not
        // index-panic. Dialog refuses to submit so the caller decides
        // what to do.
        let mut d = RestartDialog::new("S", "default", "claude", vec![], tools());
        let result = d.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Continue));
    }
}
