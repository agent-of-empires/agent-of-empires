//! Profile picker dialog - list, create, and delete profiles

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::DialogResult;
use crate::tui::components::set_prefixed_input_cursor_position;
use crate::tui::styles::Theme;

pub enum ProfilePickerAction {
    Switch(String),
    Created(String),
    Deleted(String),
}

enum Mode {
    List,
    CreateInput,
    ConfirmDelete,
}

pub struct ProfileEntry {
    pub name: String,
    pub session_count: usize,
    pub is_active: bool,
}

pub struct ProfilePickerDialog {
    mode: Mode,
    profiles: Vec<ProfileEntry>,
    selected: usize,
    name_input: Input,
    error: Option<String>,
    confirm_selected: bool,
}

impl ProfilePickerDialog {
    pub fn new(profiles: Vec<ProfileEntry>, active_profile: &str) -> Self {
        let selected = profiles
            .iter()
            .position(|p| p.name == active_profile)
            .unwrap_or(0);
        Self {
            mode: Mode::List,
            profiles,
            selected,
            name_input: Input::default(),
            error: None,
            confirm_selected: false,
        }
    }

    fn selected_profile(&self) -> Option<&ProfileEntry> {
        self.profiles.get(self.selected)
    }

    fn can_delete_selected(&self) -> bool {
        // The invariant is a count, not a name: any non-active profile is
        // deletable while it is not the last. `delete_profile` agrees.
        self.selected_profile()
            .is_some_and(|p| !p.is_active && self.profiles.len() > 1)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<ProfilePickerAction> {
        match self.mode {
            Mode::List => self.handle_list_key(key),
            Mode::CreateInput => self.handle_create_key(key),
            Mode::ConfirmDelete => self.handle_confirm_delete_key(key),
        }
    }

    fn handle_list_key(&mut self, key: KeyEvent) -> DialogResult<ProfilePickerAction> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if !self.profiles.is_empty() && self.selected < self.profiles.len() - 1 {
                    self.selected += 1;
                }
                DialogResult::Continue
            }
            KeyCode::Enter => {
                if let Some(profile) = self.selected_profile() {
                    if profile.is_active {
                        DialogResult::Cancel
                    } else {
                        DialogResult::Submit(ProfilePickerAction::Switch(profile.name.clone()))
                    }
                } else {
                    DialogResult::Cancel
                }
            }
            KeyCode::Char('n') => {
                self.mode = Mode::CreateInput;
                self.name_input = Input::default();
                self.error = None;
                DialogResult::Continue
            }
            KeyCode::Char('d') => {
                if self.can_delete_selected() {
                    self.mode = Mode::ConfirmDelete;
                    self.confirm_selected = false;
                }
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn handle_create_key(&mut self, key: KeyEvent) -> DialogResult<ProfilePickerAction> {
        match key.code {
            KeyCode::Esc => {
                self.mode = Mode::List;
                self.error = None;
                DialogResult::Continue
            }
            KeyCode::Enter => {
                let name = self.name_input.value().trim().to_string();
                if let Some(err) = self.validate_name(&name) {
                    self.error = Some(err);
                    return DialogResult::Continue;
                }
                DialogResult::Submit(ProfilePickerAction::Created(name))
            }
            _ => {
                self.name_input
                    .handle_event(&crossterm::event::Event::Key(key));
                self.error = None;
                DialogResult::Continue
            }
        }
    }

    fn handle_confirm_delete_key(&mut self, key: KeyEvent) -> DialogResult<ProfilePickerAction> {
        match key.code {
            KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                self.mode = Mode::List;
                DialogResult::Continue
            }
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if let Some(profile) = self.selected_profile() {
                    let name = profile.name.clone();
                    DialogResult::Submit(ProfilePickerAction::Deleted(name))
                } else {
                    self.mode = Mode::List;
                    DialogResult::Continue
                }
            }
            KeyCode::Enter => {
                if self.confirm_selected {
                    if let Some(profile) = self.selected_profile() {
                        let name = profile.name.clone();
                        return DialogResult::Submit(ProfilePickerAction::Deleted(name));
                    }
                }
                self.mode = Mode::List;
                DialogResult::Continue
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.confirm_selected = true;
                DialogResult::Continue
            }
            KeyCode::Right | KeyCode::Char('l') => {
                self.confirm_selected = false;
                DialogResult::Continue
            }
            KeyCode::Tab => {
                self.confirm_selected = !self.confirm_selected;
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    fn validate_name(&self, name: &str) -> Option<String> {
        if name.is_empty() {
            return Some("Profile name cannot be empty".to_string());
        }
        if name.contains('/') || name.contains('\\') {
            return Some("Profile name cannot contain path separators".to_string());
        }
        if self.profiles.iter().any(|p| p.name == name) {
            return Some(format!("Profile '{}' already exists", name));
        }
        None
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        match self.mode {
            Mode::List => self.render_list(frame, area, theme),
            Mode::CreateInput => self.render_create(frame, area, theme),
            Mode::ConfirmDelete => self.render_confirm_delete(frame, area, theme),
        }
    }

    fn render_list(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let max_visible: usize = 8;
        let list_height = self.profiles.len().min(max_visible) as u16;
        // list + hint (1) + borders (2) + margin (2)
        let dialog_height = (list_height + 5).min(area.height);
        let dialog_width: u16 = 40;

        let block = super::dialog_block(" Profiles ", theme);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Min(1),    // profile list
                Constraint::Length(1), // hint
            ])
            .split(inner);

        // Profile list with scrolling
        let visible_height = chunks[0].height as usize;
        let scroll_offset = if self.selected >= visible_height {
            self.selected - visible_height + 1
        } else {
            0
        };

        let mut lines: Vec<Line> = Vec::new();
        for (i, profile) in self
            .profiles
            .iter()
            .skip(scroll_offset)
            .take(visible_height)
            .enumerate()
        {
            let abs_idx = i + scroll_offset;
            let is_selected = abs_idx == self.selected;
            let prefix = if is_selected { "> " } else { "  " };

            let mut spans = Vec::new();
            let name_style = if is_selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            spans.push(Span::styled(prefix, name_style));
            spans.push(Span::styled(&profile.name, name_style));

            if profile.is_active {
                spans.push(Span::styled(
                    "  (active)",
                    Style::default().fg(theme.running),
                ));
            } else {
                let count_text = format!(
                    "  {} session{}",
                    profile.session_count,
                    if profile.session_count == 1 { "" } else { "s" }
                );
                spans.push(Span::styled(count_text, Style::default().fg(theme.dimmed)));
            }

            lines.push(Line::from(spans));
        }

        frame.render_widget(Paragraph::new(lines), chunks[0]);

        // Hint line
        let mut hint_spans = vec![
            Span::styled("n", Style::default().fg(theme.hint)),
            Span::raw(" new  "),
        ];
        if self.can_delete_selected() {
            hint_spans.extend([
                Span::styled("d", Style::default().fg(theme.hint)),
                Span::raw(" delete  "),
            ]);
        }
        hint_spans.extend([
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" switch  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" close"),
        ]);
        frame.render_widget(Paragraph::new(Line::from(hint_spans)), chunks[1]);
    }

    fn render_create(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let has_error = self.error.is_some();
        let dialog_width: u16 = 40;
        // inner width = dialog_width - borders(2) - margin(2) = 36
        let error_lines: u16 = if let Some(err) = &self.error {
            let inner_width = dialog_width.saturating_sub(4) as usize;
            if inner_width == 0 {
                1
            } else {
                err.len().div_ceil(inner_width) as u16
            }
        } else {
            0
        };
        // name(1) + spacer(1) + error_lines + hint(1) + borders(2) + margin(2)
        let dialog_height: u16 = if has_error { 7 + error_lines } else { 7 };

        let block = super::dialog_block(" New Profile ", theme);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let mut constraints = vec![
            Constraint::Length(1), // "Name:" label + input
            Constraint::Length(1), // spacer
        ];
        if has_error {
            constraints.push(Constraint::Length(error_lines)); // error message
        }
        constraints.push(Constraint::Length(1)); // hint

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints(constraints)
            .split(inner);

        let value = self.name_input.value();
        let input_line = Line::from(vec![
            Span::styled("Name: ", Style::default().fg(theme.text)),
            Span::styled(value, Style::default().fg(theme.accent).bold()),
            Span::styled("_", Style::default().fg(theme.accent)),
        ]);
        frame.render_widget(Paragraph::new(input_line), chunks[0]);
        set_prefixed_input_cursor_position(frame, chunks[0], "Name: ", &self.name_input);

        let mut chunk_idx = 2;

        if let Some(err) = &self.error {
            frame.render_widget(
                Paragraph::new(err.as_str())
                    .style(Style::default().fg(theme.error))
                    .wrap(Wrap { trim: true }),
                chunks[chunk_idx],
            );
            chunk_idx += 1;
        }

        let hint_line = Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" confirm  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" cancel"),
        ]);
        frame.render_widget(Paragraph::new(hint_line), chunks[chunk_idx]);
    }

    fn render_confirm_delete(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_height: u16 = 8;
        let dialog_width: u16 = 40;

        let block = super::toned_dialog_block(" Delete Profile ", theme.error, theme.error);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        if let Some(profile) = self.selected_profile() {
            let msg = format!(
                "Delete '{}' ({} session{})?",
                profile.name,
                profile.session_count,
                if profile.session_count == 1 { "" } else { "s" }
            );
            frame.render_widget(
                Paragraph::new(msg)
                    .style(Style::default().fg(theme.text))
                    .wrap(Wrap { trim: true }),
                chunks[0],
            );
        }

        let yes_style = if self.confirm_selected {
            Style::default().fg(theme.error).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };
        let no_style = if !self.confirm_selected {
            Style::default().fg(theme.running).bold()
        } else {
            Style::default().fg(theme.dimmed)
        };

        let buttons = Line::from(vec![
            Span::raw("  "),
            Span::styled("[Yes]", yes_style),
            Span::raw("    "),
            Span::styled("[No]", no_style),
        ]);
        frame.render_widget(
            Paragraph::new(buttons).alignment(Alignment::Center),
            chunks[1],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::key;

    fn entry(name: &str, session_count: usize, is_active: bool) -> ProfileEntry {
        ProfileEntry {
            name: name.to_string(),
            session_count,
            is_active,
        }
    }

    /// default (active), work, personal.
    fn dialog() -> ProfilePickerDialog {
        ProfilePickerDialog::new(
            vec![
                entry("default", 2, true),
                entry("work", 3, false),
                entry("personal", 0, false),
            ],
            "default",
        )
    }

    /// The dialog in create mode with `name` typed into it.
    fn naming(name: &str) -> ProfilePickerDialog {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(d.mode, Mode::CreateInput));
        for c in name.chars() {
            d.handle_key(key(KeyCode::Char(c)));
        }
        d
    }

    #[test]
    fn the_list_opens_on_the_active_profile_and_navigation_clamps() {
        assert_eq!(dialog().selected, 0);
        assert_eq!(
            ProfilePickerDialog::new(
                vec![entry("default", 2, true), entry("work", 3, false)],
                "work"
            )
            .selected,
            1
        );

        let mut d = dialog();
        d.handle_key(key(KeyCode::Up));
        assert_eq!(d.selected, 0, "cannot go above the first row");
        for _ in 0..4 {
            d.handle_key(key(KeyCode::Down));
        }
        assert_eq!(d.selected, 2, "cannot go past the last row");
        d.handle_key(key(KeyCode::Char('k')));
        assert_eq!(d.selected, 1);
        d.handle_key(key(KeyCode::Char('j')));
        assert_eq!(d.selected, 2);

        assert!(matches!(
            dialog().handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn enter_switches_to_another_profile_and_cancels_on_the_active_one() {
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));

        d.handle_key(key(KeyCode::Down));
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(ProfilePickerAction::Switch(name)) if name == "work"
        ));
    }

    #[test]
    fn creating_a_profile_validates_the_typed_name() {
        assert!(matches!(
            naming("test").handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(ProfilePickerAction::Created(name)) if name == "test"
        ));

        // (typed name, the phrase the inline error must carry)
        for (name, expected) in [
            ("", "empty"),
            ("work", "already exists"),
            ("a/b", "path separators"),
        ] {
            let mut d = naming(name);
            assert!(matches!(
                d.handle_key(key(KeyCode::Enter)),
                DialogResult::Continue
            ));
            let error = d.error.as_ref().expect("an inline error");
            assert!(error.contains(expected), "{name:?} -> {error:?}");
        }

        let mut d = naming("test");
        d.handle_key(key(KeyCode::Esc));
        assert!(matches!(d.mode, Mode::List));
    }

    #[test]
    fn deleting_asks_first_and_is_refused_for_the_active_or_last_profile() {
        let mut d = dialog();
        d.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(d.mode, Mode::List), "the active profile stays");

        // The invariant is a count, not a name: a profile named "default" is
        // deletable, and the last one is not, whatever it is called.
        let mut d = ProfilePickerDialog::new(
            vec![entry("default", 0, false), entry("work", 0, true)],
            "work",
        );
        d.handle_key(key(KeyCode::Up));
        assert_eq!(d.selected, 0);
        d.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(d.mode, Mode::ConfirmDelete));

        let mut d = ProfilePickerDialog::new(vec![entry("only", 0, false)], "work");
        d.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(d.mode, Mode::List));

        // Confirming deletes; Esc backs out to the list.
        let mut d = dialog();
        d.handle_key(key(KeyCode::Down));
        d.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(d.mode, Mode::ConfirmDelete));
        d.handle_key(key(KeyCode::Esc));
        assert!(matches!(d.mode, Mode::List));

        d.handle_key(key(KeyCode::Char('d')));
        assert!(matches!(
            d.handle_key(key(KeyCode::Char('y'))),
            DialogResult::Submit(ProfilePickerAction::Deleted(name)) if name == "work"
        ));
    }
}
