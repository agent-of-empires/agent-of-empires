//! Single-choice picker over a fixed option list (sort order, group-by mode).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::config::{GroupByMode, SortOrder};
use crate::tui::styles::Theme;

pub struct OptionPickerDialog<T: 'static> {
    title: &'static str,
    options: &'static [T],
    label: fn(T) -> &'static str,
    selected: usize,
    current: T,
    list_area: Rect,
    dialog_area: Rect,
}

pub type SortPickerDialog = OptionPickerDialog<SortOrder>;
pub type GroupPickerDialog = OptionPickerDialog<GroupByMode>;

impl SortPickerDialog {
    pub fn new(current: SortOrder) -> Self {
        use SortOrder::*;
        const OPTIONS: &[SortOrder] = &[Newest, Attention, LastActivity, Oldest, AZ, ZA];
        Self::with_options(" Sort Order ", OPTIONS, SortOrder::label, current)
    }
}

impl GroupPickerDialog {
    pub fn new(current: GroupByMode) -> Self {
        use GroupByMode::*;
        Self::with_options(
            " Group By ",
            &[Manual, Project, Org],
            GroupByMode::label,
            current,
        )
    }
}

impl<T: Copy + PartialEq> OptionPickerDialog<T> {
    fn with_options(
        title: &'static str,
        options: &'static [T],
        label: fn(T) -> &'static str,
        current: T,
    ) -> Self {
        Self {
            title,
            options,
            label,
            selected: options.iter().position(|o| *o == current).unwrap_or(0),
            current,
            list_area: Rect::default(),
            dialog_area: Rect::default(),
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<T> {
        if !super::contains(self.dialog_area, col, row) {
            return DialogResult::Cancel;
        }
        match super::row_index(self.list_area, col, row, self.options.len()) {
            Some(idx) => {
                self.selected = idx;
                DialogResult::Submit(self.options[idx])
            }
            None => DialogResult::Continue,
        }
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let hovered = super::row_index(self.list_area, col, row, self.options.len());
        super::hover_select(&mut self.selected, hovered)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<T> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                super::navigate_list(&mut self.selected, self.options.len(), key.code);
                DialogResult::Continue
            }
            KeyCode::Enter => DialogResult::Submit(self.options[self.selected]),
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let widest = self
            .options
            .iter()
            .map(|o| (self.label)(*o).chars().count())
            .max()
            .unwrap_or(0) as u16;
        let width = (widest + 16).clamp(32, 60);
        let height = self.options.len() as u16 + 5;
        let block = super::dialog_block(self.title, theme);
        let (dialog, inner) = super::render_dialog_frame(frame, area, width, height, block);
        self.dialog_area = dialog;

        let [list, hint] = Layout::vertical([Constraint::Min(1), Constraint::Length(1)])
            .margin(1)
            .areas(inner);

        let lines: Vec<Line> = self
            .options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let (prefix, style) = if i == self.selected {
                    ("> ", Style::default().fg(theme.accent).bold())
                } else {
                    ("  ", Style::default().fg(theme.text))
                };
                let mut spans = vec![
                    Span::styled(prefix, style),
                    Span::styled((self.label)(*option), style),
                ];
                if *option == self.current {
                    spans.push(Span::styled(
                        "  (current)",
                        Style::default().fg(theme.running),
                    ));
                }
                Line::from(spans)
            })
            .collect();
        self.list_area = list;
        frame.render_widget(Paragraph::new(lines), list);
        let hints = super::hint_line(theme, &[("Enter", "select"), ("Esc", "close")]);
        frame.render_widget(Paragraph::new(hints), hint);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::key;

    #[test]
    fn selects_current_and_submits_navigated_option() {
        assert_eq!(SortPickerDialog::new(SortOrder::LastActivity).selected, 2);
        assert_eq!(GroupPickerDialog::new(GroupByMode::Org).selected, 2);

        let mut dialog = SortPickerDialog::new(SortOrder::Newest);
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        dialog.handle_key(key(KeyCode::Down));
        dialog.handle_key(key(KeyCode::Down));
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(SortOrder::LastActivity)
        ));

        let mut dialog = GroupPickerDialog::new(GroupByMode::Manual);
        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.selected, 0);
        for _ in 0..5 {
            dialog.handle_key(key(KeyCode::Down));
        }
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(GroupByMode::Org)
        ));
    }
}
