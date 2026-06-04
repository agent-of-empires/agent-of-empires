//! Group-by picker dialog - choose a `GroupByMode` from a list.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::config::GroupByMode;
use crate::tui::styles::Theme;

const OPTIONS: &[GroupByMode] = &[GroupByMode::Manual, GroupByMode::Project];

pub struct GroupPickerDialog {
    selected: usize,
    current: GroupByMode,
    list_area: Rect,
    dialog_area: Rect,
}

impl GroupPickerDialog {
    pub fn new(current: GroupByMode) -> Self {
        let selected = OPTIONS.iter().position(|m| *m == current).unwrap_or(0);
        Self {
            selected,
            current,
            list_area: Rect::default(),
            dialog_area: Rect::default(),
        }
    }

    fn row_to_idx(&self, col: u16, row: u16) -> Option<usize> {
        if !self
            .list_area
            .contains(ratatui::layout::Position::from((col, row)))
        {
            return None;
        }
        let i = (row - self.list_area.y) as usize;
        if i >= OPTIONS.len() {
            return None;
        }
        Some(i)
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<GroupByMode> {
        if !self
            .dialog_area
            .contains(ratatui::layout::Position::from((col, row)))
        {
            return DialogResult::Cancel;
        }
        let Some(idx) = self.row_to_idx(col, row) else {
            return DialogResult::Continue;
        };
        self.selected = idx;
        DialogResult::Submit(OPTIONS[idx])
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let Some(idx) = self.row_to_idx(col, row) else {
            return false;
        };
        if self.selected == idx {
            return false;
        }
        self.selected = idx;
        true
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<GroupByMode> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < OPTIONS.len() {
                    self.selected += 1;
                }
                DialogResult::Continue
            }
            KeyCode::Enter => DialogResult::Submit(OPTIONS[self.selected]),
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width: u16 = 32;
        // list (OPTIONS.len()) + hint (1) + borders (2) + margin (2)
        let dialog_height: u16 = OPTIONS.len() as u16 + 5;

        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);
        self.dialog_area = dialog_area;
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Group By ")
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        let mut lines: Vec<Line> = Vec::new();
        for (i, mode) in OPTIONS.iter().enumerate() {
            let is_selected = i == self.selected;
            let prefix = if is_selected { "> " } else { "  " };
            let name_style = if is_selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            let mut spans = vec![
                Span::styled(prefix, name_style),
                Span::styled(mode.label(), name_style),
            ];
            if *mode == self.current {
                spans.push(Span::styled(
                    "  (current)",
                    Style::default().fg(theme.running),
                ));
            }
            lines.push(Line::from(spans));
        }
        self.list_area = chunks[0];
        frame.render_widget(Paragraph::new(lines), chunks[0]);

        let hint = Line::from(vec![
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" select  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" close"),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn test_new_selects_current() {
        let dialog = GroupPickerDialog::new(GroupByMode::Project);
        assert_eq!(dialog.selected, 1);
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = GroupPickerDialog::new(GroupByMode::Manual);
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn test_enter_submits_current_selection() {
        let mut dialog = GroupPickerDialog::new(GroupByMode::Manual);
        dialog.handle_key(key(KeyCode::Down));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(GroupByMode::Project)));
    }

    #[test]
    fn test_navigation_clamps() {
        let mut dialog = GroupPickerDialog::new(GroupByMode::Manual);
        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.selected, 0);
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(dialog.selected, 1);
    }
}
