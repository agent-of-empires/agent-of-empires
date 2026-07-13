//! Edit-worktree-workdir-name dialog.
//!
//! A focused dialog (separate from the title/group rename flow) for changing
//! a managed worktree session's directory name, with an opt-in to also rename
//! the git branch. See #1723.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::DialogResult;
use crate::tui::components::render_text_field;
use crate::tui::styles::Theme;

/// Data returned when the dialog is submitted.
#[derive(Debug, Clone)]
pub struct WorktreeNameData {
    /// New workdir name (raw; sanitized downstream).
    pub name: String,
    /// Whether to also rename the underlying git branch.
    pub rename_branch: bool,
}

pub struct WorktreeNameDialog {
    current_dir: String,
    current_branch: String,
    new_name: Input,
    rename_branch: bool,
    /// 0 = name input, 1 = "rename branch" toggle.
    focused_field: usize,
}

impl WorktreeNameDialog {
    pub fn new(current_dir: &str, current_branch: &str) -> Self {
        Self {
            current_dir: current_dir.to_string(),
            current_branch: current_branch.to_string(),
            new_name: Input::default(),
            rename_branch: false,
            focused_field: 0,
        }
    }

    fn toggle_focused(&self) -> bool {
        self.focused_field == 1
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<WorktreeNameData> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => {
                let name = self.new_name.value().trim().to_string();
                if name.is_empty() {
                    return DialogResult::Cancel;
                }
                DialogResult::Submit(WorktreeNameData {
                    name,
                    rename_branch: self.rename_branch,
                })
            }
            KeyCode::Tab | KeyCode::Down => {
                self.focused_field = (self.focused_field + 1) % 2;
                DialogResult::Continue
            }
            KeyCode::BackTab | KeyCode::Up => {
                self.focused_field = if self.focused_field == 0 { 1 } else { 0 };
                DialogResult::Continue
            }
            KeyCode::Char(' ') if self.toggle_focused() => {
                self.rename_branch = !self.rename_branch;
                DialogResult::Continue
            }
            _ => {
                if !self.toggle_focused() {
                    self.new_name
                        .handle_event(&crossterm::event::Event::Key(key));
                }
                DialogResult::Continue
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.toggle_focused() {
            return;
        }
        super::paste_into_input(&mut self.new_name, text);
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_area = super::centered_rect(area, 54, 13);
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .padding(Padding::horizontal(1))
            .border_style(Style::default().fg(theme.accent))
            .title(" Edit Workdir Name ")
            .title_style(Style::default().fg(theme.title).bold());
        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(1), // current dir
                Constraint::Length(1), // current branch
                Constraint::Length(1), // spacer
                Constraint::Length(1), // new name field
                Constraint::Length(1), // rename-branch toggle
                Constraint::Length(1), // spacer
                Constraint::Min(1),    // hint
            ])
            .split(inner);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Current dir:    ", Style::default().fg(theme.dimmed)),
                Span::styled(&self.current_dir, Style::default().fg(theme.text)),
            ])),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("Current branch: ", Style::default().fg(theme.dimmed)),
                Span::styled(&self.current_branch, Style::default().fg(theme.text)),
            ])),
            chunks[1],
        );

        render_text_field(
            frame,
            chunks[3],
            "New name:",
            &self.new_name,
            self.focused_field == 0,
            None,
            theme,
        );

        let checkbox = if self.rename_branch { "[x]" } else { "[ ]" };
        let toggle_style = if self.toggle_focused() {
            Style::default().fg(theme.accent)
        } else {
            Style::default().fg(theme.text)
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!("{checkbox} "), toggle_style),
                Span::styled("Also rename git branch", toggle_style),
            ])),
            chunks[4],
        );

        let hint = Line::from(vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" switch  "),
            Span::styled("Space", Style::default().fg(theme.hint)),
            Span::raw(" toggle  "),
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" save  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" cancel"),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[6]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    #[test]
    fn empty_name_cancels() {
        let mut d = WorktreeNameDialog::new("old", "old");
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn types_name_and_submits() {
        let mut d = WorktreeNameDialog::new("old", "old");
        for c in "new-name".chars() {
            d.handle_key(key(KeyCode::Char(c)));
        }
        match d.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(data) => {
                assert_eq!(data.name, "new-name");
                assert!(!data.rename_branch);
            }
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn toggle_enables_branch_rename() {
        let mut d = WorktreeNameDialog::new("old", "old");
        for c in "x".chars() {
            d.handle_key(key(KeyCode::Char(c)));
        }
        d.handle_key(key(KeyCode::Tab)); // focus toggle
        d.handle_key(key(KeyCode::Char(' '))); // toggle on
        match d.handle_key(key(KeyCode::Enter)) {
            DialogResult::Submit(data) => assert!(data.rename_branch),
            _ => panic!("expected submit"),
        }
    }

    #[test]
    fn space_on_name_field_types_space_not_toggle() {
        let mut d = WorktreeNameDialog::new("old", "old");
        d.handle_key(key(KeyCode::Char('a')));
        d.handle_key(key(KeyCode::Char(' ')));
        d.handle_key(key(KeyCode::Char('b')));
        assert_eq!(d.new_name.value(), "a b");
        assert!(!d.rename_branch);
    }
}
