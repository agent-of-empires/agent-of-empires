//! Rename session / group dialog

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::DialogResult;
use crate::tui::components::{
    render_text_field, render_text_field_with_ghost, GroupGhostCompletion, ListPicker,
    ListPickerResult,
};
use crate::tui::styles::Theme;

/// Data returned when the rename dialog is submitted
#[derive(Debug, Clone)]
pub struct RenameData {
    /// New title (empty string means keep current)
    pub title: String,
    /// New group path (None means keep current, Some("") means remove from group)
    pub group: Option<String>,
    /// New profile (None means keep current, Some(name) means move to that profile)
    pub profile: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameMode {
    Session,
    Group,
}

pub struct RenameDialog {
    mode: RenameMode,
    current_title: String,
    current_group: String,
    current_profile: String,
    available_profiles: Vec<String>,
    new_title: Input,
    new_group: Input,
    profile_index: usize,
    focused_field: usize, // Session: 0=title, 1=group, 2=profile; Group: 0=group, 1=profile
    existing_groups: Vec<String>,
    group_picker: ListPicker,
    group_ghost: Option<GroupGhostCompletion>,
    /// Inline validation error shown in Group mode when a duplicate name is entered.
    validation_error: Option<String>,
    /// Hit rect per focusable field (title / group / profile), set by
    /// `render`. Drives click + hover routing.
    focusable_rects: Vec<(usize, Rect)>,
}

impl RenameDialog {
    pub fn mode(&self) -> RenameMode {
        self.mode
    }

    pub fn new(
        current_title: &str,
        current_group: &str,
        current_profile: &str,
        available_profiles: Vec<String>,
        existing_groups: Vec<String>,
    ) -> Self {
        let profile_index = available_profiles
            .iter()
            .position(|p| p == current_profile)
            .unwrap_or(0);

        Self {
            mode: RenameMode::Session,
            current_title: current_title.to_string(),
            current_group: current_group.to_string(),
            current_profile: current_profile.to_string(),
            available_profiles,
            new_title: Input::default(),
            new_group: Input::new(current_group.to_string()),
            profile_index,
            focused_field: 0,
            existing_groups,
            group_picker: ListPicker::new("Select Group"),
            group_ghost: None,
            validation_error: None,
            focusable_rects: Vec::new(),
        }
    }

    pub fn new_for_group(
        current_group: &str,
        current_profile: &str,
        available_profiles: Vec<String>,
        existing_groups: Vec<String>,
    ) -> Self {
        let profile_index = available_profiles
            .iter()
            .position(|p| p == current_profile)
            .unwrap_or(0);

        Self {
            mode: RenameMode::Group,
            current_title: String::new(),
            current_group: current_group.to_string(),
            current_profile: current_profile.to_string(),
            available_profiles,
            new_title: Input::default(),
            new_group: Input::new(current_group.to_string()),
            profile_index,
            focused_field: 0,
            existing_groups,
            group_picker: ListPicker::new("Select Group"),
            group_ghost: None,
            validation_error: None,
            focusable_rects: Vec::new(),
        }
    }

    fn field_count(&self) -> usize {
        match self.mode {
            RenameMode::Session => 3, // title, group, profile
            RenameMode::Group => 2,   // group, profile
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<DialogResult<RenameData>> {
        // Group picker overlay wins when active so a click can pick a
        // group row without dropping the dialog underneath.
        if self.group_picker.is_active() {
            match self.group_picker.handle_click(col, row) {
                ListPickerResult::Continue => return Some(DialogResult::Continue),
                ListPickerResult::Cancelled => return Some(DialogResult::Continue),
                ListPickerResult::Selected(value) => {
                    self.new_group = Input::new(value);
                    // Mirror the keyboard picker path: the ghost
                    // autocomplete state goes stale once the user
                    // commits to a value via the picker, so drop it.
                    self.group_ghost = None;
                    return Some(DialogResult::Continue);
                }
            }
        }
        let pos = ratatui::layout::Position::from((col, row));
        let hit = self
            .focusable_rects
            .iter()
            .find(|(_, rect)| rect.contains(pos))
            .map(|(f, _)| *f)?;
        self.focused_field = hit;
        // Cycle the profile chip on click; text fields just take focus.
        if self.is_profile_field() && !self.available_profiles.is_empty() {
            self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
        }
        Some(DialogResult::Continue)
    }

    /// Hover only updates the group-picker overlay highlight (menu-style
    /// behavior the user expects). It deliberately does NOT move focus
    /// between the title / group / profile rows: stealing focus from the
    /// field the user is typing into just because the mouse cursor
    /// drifts across the dialog is jarring. Click still sets focus.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        if self.group_picker.is_active() {
            return self.group_picker.handle_hover(col, row);
        }
        false
    }

    fn is_profile_field(&self) -> bool {
        match self.mode {
            RenameMode::Session => self.focused_field == 2,
            RenameMode::Group => self.focused_field == 1,
        }
    }

    fn focused_input(&mut self) -> Option<&mut Input> {
        match self.mode {
            RenameMode::Session => match self.focused_field {
                0 => Some(&mut self.new_title),
                1 => Some(&mut self.new_group),
                _ => None,
            },
            RenameMode::Group => match self.focused_field {
                0 => Some(&mut self.new_group),
                _ => None,
            },
        }
    }

    fn is_group_field(&self) -> bool {
        match self.mode {
            RenameMode::Session => self.focused_field == 1,
            RenameMode::Group => self.focused_field == 0,
        }
    }

    fn next_field(&mut self) {
        self.focused_field = (self.focused_field + 1) % self.field_count();
    }

    fn prev_field(&mut self) {
        let count = self.field_count();
        self.focused_field = if self.focused_field == 0 {
            count - 1
        } else {
            self.focused_field - 1
        };
    }

    fn recompute_group_ghost(&mut self) {
        self.group_ghost = GroupGhostCompletion::compute(&self.new_group, &self.existing_groups);
    }

    fn accept_group_ghost(&mut self) {
        if let Some(ghost) = self.group_ghost.take() {
            if let Some(new_value) = ghost.accept(&self.new_group) {
                self.new_group = Input::new(new_value);
                self.recompute_group_ghost();
            }
        }
    }

    fn group_ghost_text(&self) -> Option<&str> {
        self.group_ghost.as_ref().map(|g| g.ghost_text())
    }

    fn selected_profile(&self) -> &str {
        &self.available_profiles[self.profile_index]
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<RenameData> {
        // Handle group picker if active
        if self.group_picker.is_active() {
            if let ListPickerResult::Selected(value) = self.group_picker.handle_key(key) {
                self.new_group = Input::new(value);
                self.group_ghost = None;
            }
            return DialogResult::Continue;
        }

        // Ctrl+P opens group picker on group field
        if key.code == KeyCode::Char('p')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.is_group_field()
            && !self.existing_groups.is_empty()
        {
            self.group_picker.activate(self.existing_groups.clone());
            return DialogResult::Continue;
        }

        // Right/End arrow at end of group input with ghost: accept ghost text
        if self.is_group_field()
            && matches!(key.code, KeyCode::Right | KeyCode::End)
            && key.modifiers == KeyModifiers::NONE
            && self.group_ghost.is_some()
        {
            let cursor = self.new_group.cursor();
            let char_len = self.new_group.value().chars().count();
            if cursor >= char_len {
                self.accept_group_ghost();
                return DialogResult::Continue;
            }
        }

        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => {
                let title_value = self.new_title.value().trim().to_string();
                let group_value = self.new_group.value().trim();
                let selected_profile = self.selected_profile();
                let profile_changed = selected_profile != self.current_profile;

                // If nothing has changed, cancel
                if title_value.is_empty() && group_value == self.current_group && !profile_changed {
                    return DialogResult::Cancel;
                }

                // Validate that the new group name does not already exist
                if self.mode == RenameMode::Group
                    && !group_value.is_empty()
                    && group_value != self.current_group
                    && self.existing_groups.iter().any(|g| g == group_value)
                {
                    self.validation_error = Some(
                        "A group with this name already exists.\nEnter a different name."
                            .to_string(),
                    );
                    return DialogResult::Continue;
                }

                // Determine the group value:
                // - Same as current means keep current group (None)
                // - Empty (and was non-empty) means remove from group (Some(""))
                // - Any other changed value means set new group
                let group = if group_value == self.current_group {
                    None
                } else if group_value.is_empty() {
                    Some(String::new())
                } else {
                    Some(group_value.to_string())
                };

                // Determine profile value
                let profile = if profile_changed {
                    Some(selected_profile.to_string())
                } else {
                    None
                };

                DialogResult::Submit(RenameData {
                    title: title_value,
                    group,
                    profile,
                })
            }
            KeyCode::Tab => {
                if key.modifiers.contains(KeyModifiers::SHIFT) {
                    self.prev_field();
                } else {
                    self.next_field();
                }
                if self.is_group_field() {
                    self.recompute_group_ghost();
                } else {
                    self.group_ghost = None;
                }
                DialogResult::Continue
            }
            KeyCode::Down => {
                self.next_field();
                if self.is_group_field() {
                    self.recompute_group_ghost();
                } else {
                    self.group_ghost = None;
                }
                DialogResult::Continue
            }
            KeyCode::Up => {
                self.prev_field();
                if self.is_group_field() {
                    self.recompute_group_ghost();
                } else {
                    self.group_ghost = None;
                }
                DialogResult::Continue
            }
            KeyCode::Left if self.is_profile_field() => {
                // Cycle profile backwards
                if self.profile_index == 0 {
                    self.profile_index = self.available_profiles.len().saturating_sub(1);
                } else {
                    self.profile_index -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char(' ') if self.is_profile_field() => {
                // Cycle profile forwards
                self.profile_index = (self.profile_index + 1) % self.available_profiles.len();
                DialogResult::Continue
            }
            _ => {
                if let Some(input) = self.focused_input() {
                    input.handle_event(&crossterm::event::Event::Key(key));
                }
                if self.is_group_field() {
                    self.recompute_group_ghost();
                    self.validation_error = None;
                }
                DialogResult::Continue
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if let Some(input) = self.focused_input() {
            let sanitized: String = text.chars().filter(|c| *c != '\n' && *c != '\r').collect();
            for ch in sanitized.chars() {
                input.handle(tui_input::InputRequest::InsertChar(ch));
            }
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        match self.mode {
            RenameMode::Session => self.render_session(frame, area, theme),
            RenameMode::Group => self.render_group(frame, area, theme),
        }
    }

    fn render_session(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.focusable_rects.clear();
        let dialog_width = 50;
        let dialog_area = super::centered_rect(area, dialog_width, 15);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Edit Session ")
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // Current title
                Constraint::Length(1), // Current group
                Constraint::Length(1), // Current profile
                Constraint::Length(1), // Spacer
                Constraint::Length(1), // New title field
                Constraint::Length(1), // New group field
                Constraint::Length(1), // Profile selector
                Constraint::Length(1), // Spacer
                Constraint::Min(1),    // Hint
            ])
            .split(inner);

        // Current title
        let current_title_line = Line::from(vec![
            Span::styled("Current title: ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.current_title, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(current_title_line), chunks[0]);

        // Current group
        self.render_current_group(frame, chunks[1], theme);

        // Current profile
        self.render_current_profile(frame, chunks[2], theme);

        // New title field
        render_text_field(
            frame,
            chunks[4],
            "New title:",
            &self.new_title,
            self.focused_field == 0,
            None,
            theme,
        );
        self.focusable_rects.push((0, chunks[4]));

        // New group field
        self.render_group_field(frame, chunks[5], theme);
        self.focusable_rects.push((1, chunks[5]));

        // Profile selector
        self.render_profile_selector(frame, chunks[6], theme);
        self.focusable_rects.push((2, chunks[6]));

        // Hint
        self.render_hints(frame, chunks[8], theme);

        // Render group picker overlay
        if self.group_picker.is_active() {
            self.group_picker.render(frame, area, theme);
        }
    }

    fn render_group(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        self.focusable_rects.clear();
        let dialog_width = 50;
        let has_error = self.validation_error.is_some();
        let dialog_height = if has_error { 16 } else { 13 };
        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(theme.accent))
            .title(" Rename Group ")
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let mut constraints = vec![
            Constraint::Length(1), // Current group
            Constraint::Length(1), // Current profile
            Constraint::Length(1), // Spacer
            Constraint::Length(1), // New group field
            Constraint::Length(1), // Profile selector
            Constraint::Length(1), // Spacer
            Constraint::Min(1),    // Hint
        ];
        if has_error {
            constraints.insert(5, Constraint::Length(2)); // Validation error (2 lines)
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        // Current group
        self.render_current_group(frame, chunks[0], theme);

        // Current profile
        self.render_current_profile(frame, chunks[1], theme);

        // New group field
        self.render_group_field(frame, chunks[3], theme);
        self.focusable_rects.push((0, chunks[3]));

        // Profile selector
        self.render_profile_selector(frame, chunks[4], theme);
        self.focusable_rects.push((1, chunks[4]));

        if has_error {
            // Validation error (two lines, one sentence each)
            let error_text: Vec<Line> = self
                .validation_error
                .as_deref()
                .unwrap_or("")
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(theme.error),
                    ))
                })
                .collect();
            frame.render_widget(Paragraph::new(error_text), chunks[5]);
            // Hint is shifted one index further
            self.render_hints(frame, chunks[7], theme);
        } else {
            // Hint
            self.render_hints(frame, chunks[6], theme);
        }

        // Render group picker overlay
        if self.group_picker.is_active() {
            self.group_picker.render(frame, area, theme);
        }
    }

    fn render_current_group(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let group_display = if self.current_group.is_empty() {
            "(none)".to_string()
        } else {
            self.current_group.clone()
        };
        let line = Line::from(vec![
            Span::styled("Current group: ", Style::default().fg(theme.dimmed)),
            Span::styled(group_display, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_current_profile(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let line = Line::from(vec![
            Span::styled("Current profile: ", Style::default().fg(theme.dimmed)),
            Span::styled(&self.current_profile, Style::default().fg(theme.text)),
        ]);
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_group_field(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let group_hint = if self.is_group_field() && !self.existing_groups.is_empty() {
            Some("Ctrl+P to browse")
        } else {
            None
        };
        render_text_field_with_ghost(
            frame,
            area,
            "New group:",
            &self.new_group,
            self.is_group_field(),
            group_hint,
            self.group_ghost_text(),
            theme,
        );
    }

    fn render_profile_selector(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let profile_focused = self.is_profile_field();
        let selected_profile = self.selected_profile();
        let profile_style = if profile_focused {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.text)
        };

        let profile_line = Line::from(vec![
            Span::styled(
                "Profile:    ",
                if profile_focused {
                    Style::default().fg(theme.accent)
                } else {
                    Style::default().fg(theme.dimmed)
                },
            ),
            Span::styled("< ", Style::default().fg(theme.dimmed)),
            Span::styled(selected_profile, profile_style),
            Span::styled(" >", Style::default().fg(theme.dimmed)),
        ]);
        frame.render_widget(Paragraph::new(profile_line), area);
    }

    fn render_hints(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let mut hint_spans = vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" switch  "),
        ];
        if self.is_group_field() && !self.existing_groups.is_empty() {
            if self.group_ghost_text().is_some() {
                hint_spans.push(Span::styled("→", Style::default().fg(theme.hint)));
                hint_spans.push(Span::raw(" accept  "));
            }
            hint_spans.push(Span::styled("C-p", Style::default().fg(theme.hint)));
            hint_spans.push(Span::raw(" groups  "));
        }
        hint_spans.push(Span::styled("Enter", Style::default().fg(theme.hint)));
        hint_spans.push(Span::raw(" save  "));
        hint_spans.push(Span::styled("Esc", Style::default().fg(theme.hint)));
        hint_spans.push(Span::raw(" cancel"));
        let hint = Line::from(hint_spans);
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

    fn default_profiles() -> Vec<String> {
        vec!["default".to_string()]
    }

    fn multi_profiles() -> Vec<String> {
        vec![
            "default".to_string(),
            "work".to_string(),
            "personal".to_string(),
        ]
    }

    #[test]
    fn test_new_dialog() {
        let dialog = RenameDialog::new(
            "Original Title",
            "work/frontend",
            "default",
            default_profiles(),
            Vec::new(),
        );
        assert_eq!(dialog.current_title, "Original Title");
        assert_eq!(dialog.current_group, "work/frontend");
        assert_eq!(dialog.current_profile, "default");
        assert_eq!(dialog.new_title.value(), "");
        assert_eq!(dialog.new_group.value(), "work/frontend"); // Pre-populated with current group
        assert_eq!(dialog.profile_index, 0);
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_new_dialog_empty_group() {
        let dialog = RenameDialog::new("Title", "", "default", default_profiles(), Vec::new());
        assert_eq!(dialog.current_group, "");
    }

    #[test]
    fn test_new_dialog_with_non_default_profile() {
        let dialog = RenameDialog::new("Title", "group", "work", multi_profiles(), Vec::new());
        assert_eq!(dialog.current_profile, "work");
        assert_eq!(dialog.profile_index, 1); // "work" is at index 1
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_with_unchanged_fields_cancels() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        // Title is empty, group is pre-populated but unchanged, profile unchanged - should cancel
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_with_title_only_submits() {
        let mut dialog = RenameDialog::new(
            "Old Title",
            "group",
            "default",
            default_profiles(),
            Vec::new(),
        );
        dialog.handle_key(key(KeyCode::Char('N')));
        dialog.handle_key(key(KeyCode::Char('e')));
        dialog.handle_key(key(KeyCode::Char('w')));

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, "New");
                assert_eq!(data.group, None); // Group unchanged
                assert_eq!(data.profile, None); // Profile unchanged
            }
            _ => panic!("Expected Submit result"),
        }
    }

    #[test]
    fn test_enter_with_group_only_submits() {
        let mut dialog = RenameDialog::new(
            "Title",
            "old-group",
            "default",
            default_profiles(),
            Vec::new(),
        );
        // Switch to group field and clear it
        dialog.handle_key(key(KeyCode::Tab));
        for _ in 0.."old-group".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        // Type new group
        for c in "new-group".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, ""); // Title unchanged
                assert_eq!(data.group, Some("new-group".to_string()));
                assert_eq!(data.profile, None); // Profile unchanged
            }
            _ => panic!("Expected Submit result"),
        }
    }

    #[test]
    fn test_enter_with_both_fields_submits() {
        let mut dialog = RenameDialog::new(
            "Old Title",
            "old-group",
            "default",
            default_profiles(),
            Vec::new(),
        );
        // Type title
        for c in "New Title".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        // Switch to group field and clear it
        dialog.handle_key(key(KeyCode::Tab));
        for _ in 0.."old-group".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        // Type new group
        for c in "new-group".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, "New Title");
                assert_eq!(data.group, Some("new-group".to_string()));
                assert_eq!(data.profile, None); // Profile unchanged
            }
            _ => panic!("Expected Submit result"),
        }
    }

    #[test]
    fn test_clearing_group_removes_from_group() {
        let mut dialog = RenameDialog::new(
            "Title",
            "some-group",
            "default",
            default_profiles(),
            Vec::new(),
        );
        // Switch to group field and clear it
        dialog.handle_key(key(KeyCode::Tab));
        // Clear the pre-populated value
        for _ in 0.."some-group".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, "");
                assert_eq!(data.group, Some(String::new())); // Empty string means ungroup
                assert_eq!(data.profile, None);
            }
            _ => panic!("Expected Submit result"),
        }
    }

    #[test]
    fn test_tab_switches_fields() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 2);

        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_shift_tab_switches_fields_backwards() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(shift_key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 2);

        dialog.handle_key(shift_key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(shift_key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_down_switches_to_next_field() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.focused_field, 2);
    }

    #[test]
    fn test_up_switches_to_previous_field() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        dialog.focused_field = 2;

        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.focused_field, 0);
    }

    #[test]
    fn test_char_input_goes_to_focused_field() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());

        // Type in title field
        dialog.handle_key(key(KeyCode::Char('a')));
        assert_eq!(dialog.new_title.value(), "a");
        assert_eq!(dialog.new_group.value(), "group"); // Pre-populated

        // Switch to group and type (appends to pre-populated value)
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Char('b')));
        assert_eq!(dialog.new_title.value(), "a");
        assert_eq!(dialog.new_group.value(), "groupb");
    }

    #[test]
    fn test_char_input_ignored_on_profile_field() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", multi_profiles(), Vec::new());
        dialog.focused_field = 2; // Profile field

        // Typing should not affect anything
        dialog.handle_key(key(KeyCode::Char('a')));
        assert_eq!(dialog.profile_index, 0);
        assert_eq!(dialog.new_title.value(), "");
        assert_eq!(dialog.new_group.value(), "group");
    }

    #[test]
    fn test_backspace_removes_char_from_focused_field() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Char('c')));

        dialog.handle_key(key(KeyCode::Backspace));
        assert_eq!(dialog.new_title.value(), "ab");
    }

    #[test]
    fn test_current_values_preserved() {
        let mut dialog = RenameDialog::new(
            "Original",
            "original-group",
            "default",
            default_profiles(),
            Vec::new(),
        );
        dialog.handle_key(key(KeyCode::Char('N')));
        dialog.handle_key(key(KeyCode::Char('e')));
        dialog.handle_key(key(KeyCode::Char('w')));

        assert_eq!(dialog.current_title, "Original");
        assert_eq!(dialog.current_group, "original-group");
        assert_eq!(dialog.current_profile, "default");
        assert_eq!(dialog.new_title.value(), "New");
    }

    #[test]
    fn test_full_workflow_type_both_and_submit() {
        let mut dialog = RenameDialog::new(
            "Old Name",
            "old/group",
            "default",
            default_profiles(),
            Vec::new(),
        );

        // Type new title
        for c in "Renamed Project".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }

        // Switch to group and clear it, then type new group
        dialog.handle_key(key(KeyCode::Tab));
        for _ in 0.."old/group".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for c in "new/group".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, "Renamed Project");
                assert_eq!(data.group, Some("new/group".to_string()));
                assert_eq!(data.profile, None);
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_full_workflow_type_and_cancel() {
        let mut dialog = RenameDialog::new(
            "Old Name",
            "group",
            "default",
            default_profiles(),
            Vec::new(),
        );

        dialog.handle_key(key(KeyCode::Char('N')));
        dialog.handle_key(key(KeyCode::Char('e')));
        dialog.handle_key(key(KeyCode::Char('w')));

        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_whitespace_is_trimmed() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        for c in "  New Title  ".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(key(KeyCode::Tab));
        // Clear pre-populated value first
        for _ in 0.."group".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for c in "  new-group  ".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, "New Title");
                assert_eq!(data.group, Some("new-group".to_string()));
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_left_right_arrow_moves_cursor_in_input() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Char('c')));

        // Move cursor left and insert
        dialog.handle_key(key(KeyCode::Left));
        dialog.handle_key(key(KeyCode::Char('X')));

        assert_eq!(dialog.new_title.value(), "abXc");
    }

    #[test]
    fn test_profile_selection_with_right_arrow() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", multi_profiles(), Vec::new());
        assert_eq!(dialog.profile_index, 0);
        assert_eq!(dialog.selected_profile(), "default");

        // Move to profile field
        dialog.focused_field = 2;

        // Cycle forward
        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.profile_index, 1);
        assert_eq!(dialog.selected_profile(), "work");

        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.profile_index, 2);
        assert_eq!(dialog.selected_profile(), "personal");

        // Wrap around
        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.profile_index, 0);
        assert_eq!(dialog.selected_profile(), "default");
    }

    #[test]
    fn test_profile_selection_with_space_key() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", multi_profiles(), Vec::new());
        dialog.focused_field = 2;

        // Space cycles forward like Right arrow
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dialog.profile_index, 1);
        assert_eq!(dialog.selected_profile(), "work");

        dialog.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dialog.profile_index, 2);
        assert_eq!(dialog.selected_profile(), "personal");

        // Wrap around
        dialog.handle_key(key(KeyCode::Char(' ')));
        assert_eq!(dialog.profile_index, 0);
        assert_eq!(dialog.selected_profile(), "default");
    }

    #[test]
    fn test_profile_selection_with_left_arrow() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", multi_profiles(), Vec::new());
        dialog.focused_field = 2;

        // Cycle backward (should wrap to end)
        dialog.handle_key(key(KeyCode::Left));
        assert_eq!(dialog.profile_index, 2);
        assert_eq!(dialog.selected_profile(), "personal");

        dialog.handle_key(key(KeyCode::Left));
        assert_eq!(dialog.profile_index, 1);
        assert_eq!(dialog.selected_profile(), "work");

        dialog.handle_key(key(KeyCode::Left));
        assert_eq!(dialog.profile_index, 0);
        assert_eq!(dialog.selected_profile(), "default");
    }

    #[test]
    fn test_profile_arrows_only_work_on_profile_field() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", multi_profiles(), Vec::new());
        assert_eq!(dialog.focused_field, 0); // Title field

        // Right arrow on title field should move cursor, not change profile
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(key(KeyCode::Char('b')));
        let initial_profile = dialog.profile_index;
        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.profile_index, initial_profile);
    }

    #[test]
    fn test_submit_with_profile_change() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", multi_profiles(), Vec::new());

        // Change profile
        dialog.focused_field = 2;
        dialog.handle_key(key(KeyCode::Right)); // Select "work"

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, "");
                assert_eq!(data.group, None);
                assert_eq!(data.profile, Some("work".to_string()));
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_submit_with_all_changes() {
        let mut dialog = RenameDialog::new(
            "Old Title",
            "old-group",
            "default",
            multi_profiles(),
            Vec::new(),
        );

        // Change title
        for c in "New Title".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }

        // Change group
        dialog.handle_key(key(KeyCode::Tab));
        for _ in 0.."old-group".len() {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for c in "new-group".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }

        // Change profile
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Right)); // Select "work"

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.title, "New Title");
                assert_eq!(data.group, Some("new-group".to_string()));
                assert_eq!(data.profile, Some("work".to_string()));
            }
            _ => panic!("Expected Submit"),
        }
    }

    #[test]
    fn test_same_profile_returns_none() {
        let mut dialog = RenameDialog::new("Test", "group", "work", multi_profiles(), Vec::new());

        // Change title to trigger submit
        dialog.handle_key(key(KeyCode::Char('X')));

        // Profile stays at "work" (don't change it)
        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.profile, None); // Same profile, returns None
            }
            _ => panic!("Expected Submit"),
        }
    }

    fn ctrl_p() -> KeyEvent {
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL)
    }

    fn sample_groups() -> Vec<String> {
        vec![
            "work".to_string(),
            "work/frontend".to_string(),
            "personal".to_string(),
        ]
    }

    #[test]
    fn test_ctrl_p_opens_group_picker_on_group_field() {
        let mut dialog = RenameDialog::new(
            "Test",
            "group",
            "default",
            default_profiles(),
            sample_groups(),
        );
        // Focus group field
        dialog.handle_key(key(KeyCode::Tab));
        assert_eq!(dialog.focused_field, 1);

        dialog.handle_key(ctrl_p());
        assert!(dialog.group_picker.is_active());
    }

    #[test]
    fn test_ctrl_p_ignored_on_title_field() {
        let mut dialog = RenameDialog::new(
            "Test",
            "group",
            "default",
            default_profiles(),
            sample_groups(),
        );
        assert_eq!(dialog.focused_field, 0);

        dialog.handle_key(ctrl_p());
        assert!(!dialog.group_picker.is_active());
    }

    #[test]
    fn test_ctrl_p_ignored_on_profile_field() {
        let mut dialog = RenameDialog::new(
            "Test",
            "group",
            "default",
            default_profiles(),
            sample_groups(),
        );
        dialog.focused_field = 2;

        dialog.handle_key(ctrl_p());
        assert!(!dialog.group_picker.is_active());
    }

    #[test]
    fn test_ctrl_p_ignored_when_no_groups() {
        let mut dialog =
            RenameDialog::new("Test", "group", "default", default_profiles(), Vec::new());
        dialog.handle_key(key(KeyCode::Tab)); // Focus group field
        dialog.handle_key(ctrl_p());
        assert!(!dialog.group_picker.is_active());
    }

    #[test]
    fn test_group_picker_select_sets_group_field() {
        let mut dialog = RenameDialog::new(
            "Test",
            "old-group",
            "default",
            default_profiles(),
            sample_groups(),
        );
        dialog.handle_key(key(KeyCode::Tab)); // Focus group field
        dialog.handle_key(ctrl_p()); // Open picker
        assert!(dialog.group_picker.is_active());

        // Select first item ("work")
        dialog.handle_key(key(KeyCode::Enter));
        assert!(!dialog.group_picker.is_active());
        assert_eq!(dialog.new_group.value(), "work");
    }

    #[test]
    fn test_group_picker_cancel_keeps_original_value() {
        let mut dialog = RenameDialog::new(
            "Test",
            "old-group",
            "default",
            default_profiles(),
            sample_groups(),
        );
        dialog.handle_key(key(KeyCode::Tab)); // Focus group field
        dialog.handle_key(ctrl_p()); // Open picker
        assert!(dialog.group_picker.is_active());

        // Cancel picker
        dialog.handle_key(key(KeyCode::Esc));
        assert!(!dialog.group_picker.is_active());
        assert_eq!(dialog.new_group.value(), "old-group");
    }

    #[test]
    fn test_group_picker_navigate_and_select() {
        let mut dialog = RenameDialog::new(
            "Test",
            "old-group",
            "default",
            default_profiles(),
            sample_groups(),
        );
        dialog.handle_key(key(KeyCode::Tab)); // Focus group field
        dialog.handle_key(ctrl_p()); // Open picker

        // Navigate down to second item ("work/frontend")
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Enter));
        assert_eq!(dialog.new_group.value(), "work/frontend");
    }

    #[test]
    fn test_group_picker_selected_value_submits_correctly() {
        let mut dialog = RenameDialog::new(
            "Test",
            "old-group",
            "default",
            default_profiles(),
            sample_groups(),
        );
        dialog.handle_key(key(KeyCode::Tab)); // Focus group field
        dialog.handle_key(ctrl_p()); // Open picker
        dialog.handle_key(key(KeyCode::Enter)); // Select "work"

        let result = dialog.handle_key(key(KeyCode::Enter));
        match result {
            DialogResult::Submit(data) => {
                assert_eq!(data.group, Some("work".to_string()));
            }
            _ => panic!("Expected Submit"),
        }
    }

    // --- Group ghost autocomplete tests ---

    #[test]
    fn test_group_ghost_appears_on_typing() {
        let mut dialog =
            RenameDialog::new("Test", "", "default", default_profiles(), sample_groups());
        dialog.handle_key(key(KeyCode::Tab)); // Focus group field
        dialog.handle_key(key(KeyCode::Char('p')));
        assert_eq!(dialog.group_ghost_text(), Some("ersonal"));
    }

    #[test]
    fn test_group_ghost_none_when_no_match() {
        let mut dialog =
            RenameDialog::new("Test", "", "default", default_profiles(), sample_groups());
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Char('z')));
        assert!(dialog.group_ghost_text().is_none());
    }

    #[test]
    fn test_group_ghost_accept_with_right_arrow() {
        let mut dialog =
            RenameDialog::new("Test", "", "default", default_profiles(), sample_groups());
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Char('p')));
        assert!(dialog.group_ghost_text().is_some());

        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.new_group.value(), "personal");
    }

    #[test]
    fn test_group_ghost_accept_with_end_key() {
        let mut dialog =
            RenameDialog::new("Test", "", "default", default_profiles(), sample_groups());
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Char('p')));
        assert!(dialog.group_ghost_text().is_some());

        dialog.handle_key(key(KeyCode::End));
        assert_eq!(dialog.new_group.value(), "personal");
    }

    #[test]
    fn test_group_ghost_cleared_on_field_switch() {
        let mut dialog =
            RenameDialog::new("Test", "", "default", default_profiles(), sample_groups());
        dialog.handle_key(key(KeyCode::Tab)); // Focus group field
        dialog.handle_key(key(KeyCode::Char('p')));
        assert!(dialog.group_ghost_text().is_some());

        dialog.handle_key(key(KeyCode::Tab)); // Move to profile field
        assert!(dialog.group_ghost_text().is_none());
    }

    #[test]
    fn test_group_ghost_common_prefix_for_multiple_matches() {
        let mut dialog =
            RenameDialog::new("Test", "", "default", default_profiles(), sample_groups());
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Char('w')));
        // "work" and "work/frontend" share common prefix "work"
        // Ghost should show "ork" (common prefix minus typed "w")
        assert_eq!(dialog.group_ghost_text(), Some("ork"));
    }

    #[test]
    fn test_group_ghost_cleared_on_picker_select() {
        let mut dialog =
            RenameDialog::new("Test", "", "default", default_profiles(), sample_groups());
        dialog.handle_key(key(KeyCode::Tab));
        dialog.handle_key(key(KeyCode::Char('w')));
        assert!(dialog.group_ghost_text().is_some());

        dialog.handle_key(ctrl_p()); // Open picker
        dialog.handle_key(key(KeyCode::Enter)); // Select "work"
        assert!(dialog.group_ghost_text().is_none());
        assert_eq!(dialog.new_group.value(), "work");
    }

    // --- Group rename duplicate validation tests ---

    fn existing_groups_with_personal() -> Vec<String> {
        vec![
            "work".to_string(),
            "personal".to_string(),
            "work/frontend".to_string(),
        ]
    }

    #[test]
    fn test_group_rename_duplicate_shows_error() {
        let mut dialog = RenameDialog::new_for_group(
            "work",
            "default",
            default_profiles(),
            existing_groups_with_personal(),
        );

        // Clear the pre-filled group name and type an existing group name
        for _ in 0..4 {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for ch in "personal".chars() {
            dialog.handle_key(key(KeyCode::Char(ch)));
        }

        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(result, DialogResult::Continue),
            "should not submit when duplicate name"
        );
        assert!(
            dialog.validation_error.is_some(),
            "validation_error should be set"
        );
    }

    #[test]
    fn test_group_rename_error_clears_on_edit() {
        let mut dialog = RenameDialog::new_for_group(
            "work",
            "default",
            default_profiles(),
            existing_groups_with_personal(),
        );

        for _ in 0..4 {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for ch in "personal".chars() {
            dialog.handle_key(key(KeyCode::Char(ch)));
        }
        dialog.handle_key(key(KeyCode::Enter));
        assert!(dialog.validation_error.is_some());

        // Any keystroke on the group field should clear the error
        dialog.handle_key(key(KeyCode::Backspace));
        assert!(
            dialog.validation_error.is_none(),
            "validation_error should clear on edit"
        );
    }

    #[test]
    fn test_group_rename_allows_own_name() {
        let mut dialog = RenameDialog::new_for_group(
            "work",
            "default",
            default_profiles(),
            existing_groups_with_personal(),
        );

        // Submitting the unchanged name should cancel (nothing changed), not error
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(result, DialogResult::Cancel),
            "unchanged name should cancel, not show duplicate error"
        );
        assert!(
            dialog.validation_error.is_none(),
            "no validation error for own name"
        );
    }

    #[test]
    fn test_group_rename_submit_new_unique_name() {
        let mut dialog = RenameDialog::new_for_group(
            "work",
            "default",
            default_profiles(),
            existing_groups_with_personal(),
        );

        for _ in 0..4 {
            dialog.handle_key(key(KeyCode::Backspace));
        }
        for ch in "projects".chars() {
            dialog.handle_key(key(KeyCode::Char(ch)));
        }

        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(result, DialogResult::Submit(_)),
            "unique name should submit"
        );
        assert!(dialog.validation_error.is_none());
    }
}
