//! Single-choice picker over a fixed option list (sort order, group-by mode).

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::session::config::{GroupByMode, SortOrder};
use crate::tui::components::hint_buttons::HintButtons;
use crate::tui::styles::Theme;

pub struct OptionPickerDialog<T: 'static> {
    title: &'static str,
    options: &'static [T],
    label: fn(T) -> &'static str,
    mnemonic: fn(T) -> char,
    selected: usize,
    current: T,
    list_area: Rect,
    dialog_area: Rect,
    footer: HintButtons,
}

/// Split `label` around the first case-insensitive occurrence of `mnemonic`, so the letter
/// that selects the option can be picked out where it already reads.
fn split_at_mnemonic(label: &str, mnemonic: char) -> Option<(&str, &str, &str)> {
    let at = label
        .char_indices()
        .find(|(_, c)| c.eq_ignore_ascii_case(&mnemonic))
        .map(|(i, _)| i)?;
    let end = at + label[at..].chars().next()?.len_utf8();
    Some((&label[..at], &label[at..end], &label[end..]))
}

pub type SortPickerDialog = OptionPickerDialog<SortOrder>;
pub type GroupPickerDialog = OptionPickerDialog<GroupByMode>;

impl SortPickerDialog {
    pub fn new(current: SortOrder) -> Self {
        use SortOrder::*;
        const OPTIONS: &[SortOrder] = &[Newest, Attention, LastActivity, Oldest, AZ, ZA, Custom];
        Self::with_options(
            " Sort Order ",
            OPTIONS,
            SortOrder::label,
            SortOrder::mnemonic,
            current,
        )
    }
}

impl GroupPickerDialog {
    pub fn new(current: GroupByMode) -> Self {
        use GroupByMode::*;
        Self::with_options(
            " Group By ",
            &[Manual, Project, Org],
            GroupByMode::label,
            GroupByMode::mnemonic,
            current,
        )
    }
}

impl<T: Copy + PartialEq> OptionPickerDialog<T> {
    fn with_options(
        title: &'static str,
        options: &'static [T],
        label: fn(T) -> &'static str,
        mnemonic: fn(T) -> char,
        current: T,
    ) -> Self {
        Self {
            title,
            options,
            label,
            mnemonic,
            selected: options.iter().position(|o| *o == current).unwrap_or(0),
            current,
            list_area: Rect::default(),
            dialog_area: Rect::default(),
            footer: HintButtons::default(),
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<T> {
        if !super::contains(self.dialog_area, col, row) {
            return DialogResult::Cancel;
        }
        if let Some(key) = self.footer.key_at(col, row) {
            return self.handle_key(key);
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
        self.footer.handle_hover(col, row) | super::hover_select(&mut self.selected, hovered)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<T> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up | KeyCode::Char('k') | KeyCode::Down | KeyCode::Char('j') => {
                super::navigate_list(&mut self.selected, self.options.len(), key.code);
                DialogResult::Continue
            }
            KeyCode::Enter => DialogResult::Submit(self.options[self.selected]),
            // Each option owns one letter, so a press picks that option outright.
            KeyCode::Char(c) => {
                let typed = c.to_ascii_lowercase();
                match self
                    .options
                    .iter()
                    .position(|o| (self.mnemonic)(*o) == typed)
                {
                    Some(idx) => {
                        self.selected = idx;
                        DialogResult::Submit(self.options[idx])
                    }
                    None => DialogResult::Continue,
                }
            }
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
                let label = (self.label)(*option);
                let mut spans = vec![Span::styled(prefix, style)];
                // The mnemonic is shown as its letter inside the label, not as a separate
                // column: one press picks the option, and the label says which press.
                match split_at_mnemonic(label, (self.mnemonic)(*option)) {
                    Some((head, letter, tail)) => {
                        spans.push(Span::styled(head, style));
                        spans.push(Span::styled(letter, style.fg(theme.accent).underlined()));
                        spans.push(Span::styled(tail, style));
                    }
                    None => spans.push(Span::styled(label, style)),
                }
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
        self.footer.render(
            frame,
            hint,
            theme,
            &[
                ("Enter", "select", KeyCode::Null),
                ("Esc", "close", KeyCode::Esc),
            ],
            Alignment::Left,
        );
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

    /// Each option owns one letter: the press selects and submits that option, and a letter
    /// no option claims leaves the dialog alone.
    #[test]
    fn a_mnemonic_letter_submits_its_option() {
        for (letter, expected) in [
            ('c', SortOrder::Custom),
            ('t', SortOrder::Attention),
            ('a', SortOrder::AZ),
            ('z', SortOrder::ZA),
            ('N', SortOrder::Newest),
        ] {
            let mut dialog = SortPickerDialog::new(SortOrder::Oldest);
            assert_eq!(
                dialog.handle_key(key(KeyCode::Char(letter))),
                DialogResult::Submit(expected),
                "{letter}"
            );
        }

        let mut dialog = SortPickerDialog::new(SortOrder::Oldest);
        let before = dialog.selected;
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('q'))),
            DialogResult::Continue
        );
        assert_eq!(dialog.selected, before);
    }
}
