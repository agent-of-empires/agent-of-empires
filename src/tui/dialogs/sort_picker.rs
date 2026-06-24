//! Sort-order picker dialog: the core `SortOrder`s. Picking an order shows
//! here as the current choice.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::config::SortOrder;
use crate::tui::styles::Theme;

const CORE_OPTIONS: &[SortOrder] = &[
    SortOrder::Newest,
    SortOrder::Attention,
    SortOrder::LastActivity,
    SortOrder::Oldest,
    SortOrder::AZ,
    SortOrder::ZA,
];

struct Row {
    order: SortOrder,
    label: String,
}

pub struct SortPickerDialog {
    rows: Vec<Row>,
    selected: usize,
    current: SortOrder,
    list_area: Rect,
    dialog_area: Rect,
}

impl SortPickerDialog {
    pub fn new(current_order: SortOrder) -> Self {
        let rows: Vec<Row> = CORE_OPTIONS
            .iter()
            .map(|o| Row {
                order: *o,
                label: o.label().to_string(),
            })
            .collect();
        let selected = rows
            .iter()
            .position(|r| r.order == current_order)
            .unwrap_or(0);
        Self {
            rows,
            selected,
            current: current_order,
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
        if i >= self.rows.len() {
            return None;
        }
        Some(i)
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<SortOrder> {
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
        DialogResult::Submit(self.rows[idx].order)
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

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<SortOrder> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up | KeyCode::Char('k') => {
                if self.selected > 0 {
                    self.selected -= 1;
                }
                DialogResult::Continue
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.selected + 1 < self.rows.len() {
                    self.selected += 1;
                }
                DialogResult::Continue
            }
            KeyCode::Enter => DialogResult::Submit(self.rows[self.selected].order),
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let widest = self
            .rows
            .iter()
            .map(|r| r.label.chars().count())
            .max()
            .unwrap_or(0) as u16;
        let dialog_width: u16 = (widest + 16).clamp(32, 60);
        let dialog_height: u16 = self.rows.len() as u16 + 5;

        let dialog_area = super::centered_rect(area, dialog_width, dialog_height);
        self.dialog_area = dialog_area;
        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(" Sort Order ")
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(1)])
            .split(inner);

        let mut lines: Vec<Line> = Vec::new();
        for (i, row) in self.rows.iter().enumerate() {
            let is_selected = i == self.selected;
            let prefix = if is_selected { "> " } else { "  " };
            let name_style = if is_selected {
                Style::default().fg(theme.accent).bold()
            } else {
                Style::default().fg(theme.text)
            };
            let mut spans = vec![
                Span::styled(prefix, name_style),
                Span::styled(row.label.clone(), name_style),
            ];
            if row.order == self.current {
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
        let dialog = SortPickerDialog::new(SortOrder::LastActivity);
        assert_eq!(dialog.selected, 2);
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = SortPickerDialog::new(SortOrder::Newest);
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
    }

    #[test]
    fn test_enter_submits_selection() {
        let mut dialog = SortPickerDialog::new(SortOrder::Newest);
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            result,
            DialogResult::Submit(SortOrder::LastActivity)
        ));
    }

    #[test]
    fn test_navigation_clamps() {
        let mut dialog = SortPickerDialog::new(SortOrder::Newest);
        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.selected, 0);
        for _ in 0..30 {
            dialog.handle_key(key(KeyCode::Down));
        }
        assert_eq!(dialog.selected, CORE_OPTIONS.len() - 1);
    }
}
