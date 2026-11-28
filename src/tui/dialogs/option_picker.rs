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
    /// The entry below the options, after a divider, that runs an action rather than
    /// picking one; `bool` is whether it may run.
    action: Option<(&'static str, bool)>,
    /// Why the options cannot be picked; the picker still shows the current one.
    locked: Option<&'static str>,
    selected: usize,
    current: T,
    list_area: Rect,
    dialog_area: Rect,
    footer: HintButtons,
}

/// What a submitted picker chose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick<T> {
    Option(T),
    Action,
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
    pub fn new(current: SortOrder, locked: Option<&'static str>) -> Self {
        Self::with_options(
            " Sort Order ",
            &[SortOrder::Custom, SortOrder::Newest, SortOrder::Name],
            SortOrder::label,
            SortOrder::mnemonic,
            current,
            locked,
        )
    }
}

impl GroupPickerDialog {
    /// `can_create` is whether "+ New group…" may run, which a read-only Daemon refuses.
    pub fn new(current: GroupByMode, locked: Option<&'static str>, can_create: bool) -> Self {
        let mut dialog = Self::with_options(
            " Group By ",
            &[GroupByMode::Project, GroupByMode::Group],
            GroupByMode::label,
            GroupByMode::mnemonic,
            current,
            locked,
        );
        dialog.action = Some(("+ New group…", can_create));
        dialog
    }
}

impl<T: Copy + PartialEq> OptionPickerDialog<T> {
    fn with_options(
        title: &'static str,
        options: &'static [T],
        label: fn(T) -> &'static str,
        mnemonic: fn(T) -> char,
        current: T,
        locked: Option<&'static str>,
    ) -> Self {
        Self {
            title,
            options,
            label,
            mnemonic,
            action: None,
            locked,
            selected: options.iter().position(|o| *o == current).unwrap_or(0),
            current,
            list_area: Rect::default(),
            dialog_area: Rect::default(),
            footer: HintButtons::default(),
        }
    }

    /// Selectable entries: the options, then the action.
    fn entries(&self) -> usize {
        self.options.len() + usize::from(self.action.is_some())
    }

    /// The entry drawn on `line` of the list, which has a divider above the action.
    fn entry_at_line(&self, line: usize) -> Option<usize> {
        let n = self.options.len();
        match line {
            line if line < n => Some(line),
            line if line == n + 1 && self.action.is_some() => Some(n),
            _ => None,
        }
    }

    fn submit(&mut self, idx: usize) -> DialogResult<Pick<T>> {
        self.selected = idx;
        match self.options.get(idx) {
            Some(_) if self.locked.is_some() => DialogResult::Continue,
            Some(option) => DialogResult::Submit(Pick::Option(*option)),
            None if self.action.is_some_and(|(_, enabled)| enabled) => {
                DialogResult::Submit(Pick::Action)
            }
            None => DialogResult::Continue,
        }
    }

    pub fn handle_click(&mut self, col: u16, row: u16) -> DialogResult<Pick<T>> {
        if !super::contains(self.dialog_area, col, row) {
            return DialogResult::Cancel;
        }
        if let Some(key) = self.footer.key_at(col, row) {
            return self.handle_key(key);
        }
        let lines = self.entries() + usize::from(self.action.is_some());
        match super::row_index(self.list_area, col, row, lines).and_then(|l| self.entry_at_line(l))
        {
            Some(idx) => self.submit(idx),
            None => DialogResult::Continue,
        }
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        let lines = self.entries() + usize::from(self.action.is_some());
        let hovered = super::row_index(self.list_area, col, row, lines)
            .and_then(|line| self.entry_at_line(line));
        self.footer.handle_hover(col, row) | super::hover_select(&mut self.selected, hovered)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<Pick<T>> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Up
            | KeyCode::Char('k')
            | KeyCode::Down
            | KeyCode::Char('j')
            | KeyCode::Home
            | KeyCode::End => {
                let entries = self.entries();
                super::navigate_list(&mut self.selected, entries, key.code);
                DialogResult::Continue
            }
            KeyCode::Enter => self.submit(self.selected),
            // Each option owns one letter, so a press picks that option outright.
            KeyCode::Char(c) => {
                let typed = c.to_ascii_lowercase();
                match self
                    .options
                    .iter()
                    .position(|o| (self.mnemonic)(*o) == typed)
                {
                    Some(idx) => self.submit(idx),
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
            .chain(self.action.map(|(label, _)| label.chars().count()))
            .chain(self.locked.map(|reason| reason.chars().count()))
            .max()
            .unwrap_or(0) as u16;
        let width = (widest + 16).clamp(32, 60);
        let extra = 2 * u16::from(self.action.is_some()) + u16::from(self.locked.is_some());
        let height = self.options.len() as u16 + extra + 5;
        let block = super::dialog_block(self.title, theme);
        let (dialog, inner) = super::render_dialog_frame(frame, area, width, height, block);
        self.dialog_area = dialog;

        let [list, reason, hint] = Layout::vertical([
            Constraint::Min(1),
            Constraint::Length(u16::from(self.locked.is_some())),
            Constraint::Length(1),
        ])
        .margin(1)
        .areas(inner);

        let row_style = |selected: bool, enabled: bool| match (selected, enabled) {
            (true, true) => ("> ", Style::default().fg(theme.accent).bold()),
            (true, false) => ("> ", Style::default().fg(theme.dimmed).bold()),
            (false, true) => ("  ", Style::default().fg(theme.text)),
            (false, false) => ("  ", Style::default().fg(theme.dimmed)),
        };
        let mut lines: Vec<Line> = self
            .options
            .iter()
            .enumerate()
            .map(|(i, option)| {
                let (prefix, style) = row_style(i == self.selected, self.locked.is_none());
                let label = (self.label)(*option);
                let mut spans = vec![Span::styled(prefix, style)];
                // The mnemonic is shown as its letter inside the label, not as a separate
                // column: one press picks the option, and the label says which press.
                match split_at_mnemonic(label, (self.mnemonic)(*option))
                    .filter(|_| self.locked.is_none())
                {
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
        if let Some((label, enabled)) = self.action {
            lines.push(Line::styled(
                "─".repeat(list.width as usize),
                Style::default().fg(theme.border),
            ));
            let (prefix, style) = row_style(self.selected == self.options.len(), enabled);
            lines.push(Line::from(vec![
                Span::styled(prefix, style),
                Span::styled(label, style),
            ]));
        }
        self.list_area = list;
        frame.render_widget(Paragraph::new(lines), list);
        if let Some(text) = self.locked {
            frame.render_widget(
                Paragraph::new(Span::styled(text, Style::default().fg(theme.waiting))),
                reason,
            );
        }
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
        assert_eq!(SortPickerDialog::new(SortOrder::Name, None).selected, 2);
        assert_eq!(
            GroupPickerDialog::new(GroupByMode::Group, None, true).selected,
            1
        );

        let mut dialog = SortPickerDialog::new(SortOrder::Custom, None);
        assert!(matches!(
            dialog.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(Pick::Option(SortOrder::Newest))
        );
        for _ in 0..5 {
            dialog.handle_key(key(KeyCode::Down));
        }
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(Pick::Option(SortOrder::Name))
        );

        let mut dialog = GroupPickerDialog::new(GroupByMode::Group, None, true);
        dialog.handle_key(key(KeyCode::Up));
        assert_eq!(dialog.selected, 0);
        dialog.handle_key(key(KeyCode::Down));
        assert_eq!(
            dialog.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(Pick::Option(GroupByMode::Group))
        );
    }

    /// "+ New group…" is the last entry of the Group By picker, reached with the arrows like
    /// the groupings and owning no letter.
    #[test]
    fn new_group_is_the_last_group_by_entry() {
        for current in [GroupByMode::Project, GroupByMode::Group] {
            let mut dialog = GroupPickerDialog::new(current, None, true);
            for _ in 0..5 {
                dialog.handle_key(key(KeyCode::Down));
            }
            assert_eq!(
                dialog.handle_key(key(KeyCode::Enter)),
                DialogResult::Submit(Pick::Action),
                "{current:?}"
            );
            assert_eq!(
                dialog.handle_key(key(KeyCode::Char('+'))),
                DialogResult::Continue
            );
        }
        let mut refused = GroupPickerDialog::new(GroupByMode::Project, None, false);
        refused.handle_key(key(KeyCode::End));
        assert_eq!(
            refused.handle_key(key(KeyCode::Enter)),
            DialogResult::Continue
        );
    }

    /// A locked picker keeps the current choice selected and refuses every pick, by Enter or
    /// by letter, while the action follows its own flag.
    #[test]
    fn a_locked_picker_refuses_picks() {
        let reason = Some("This Daemon is read-only.");
        let mut sort = SortPickerDialog::new(SortOrder::Newest, reason);
        assert_eq!(sort.selected, 1);
        for code in [
            KeyCode::Enter,
            KeyCode::Char('a'),
            KeyCode::Down,
            KeyCode::Enter,
        ] {
            assert_eq!(
                sort.handle_key(key(code)),
                DialogResult::Continue,
                "{code:?}"
            );
        }
        let mut group =
            GroupPickerDialog::new(GroupByMode::Group, Some("Managed by CityHall."), true);
        assert_eq!(
            group.handle_key(key(KeyCode::Char('p'))),
            DialogResult::Continue
        );
        group.handle_key(key(KeyCode::End));
        assert_eq!(
            group.handle_key(key(KeyCode::Enter)),
            DialogResult::Submit(Pick::Action)
        );
    }

    /// The drawn picker shows the reason it is locked, and a click resolves past the divider
    /// to "+ New group…".
    #[test]
    fn a_click_reaches_the_action_below_the_divider() {
        use crate::tui::dialogs::test_render::{draw, find, text};
        let mut dialog =
            GroupPickerDialog::new(GroupByMode::Project, Some("Managed by CityHall."), true);
        let buf = draw(60, 20, |f, theme| dialog.render(f, f.area(), theme));
        assert!(text(&buf).contains("Managed by CityHall."));
        let (col, row) = find(&buf, "Groups");
        assert_eq!(dialog.handle_click(col, row), DialogResult::Continue);
        assert_eq!(dialog.handle_click(col, row + 1), DialogResult::Continue);
        let (col, row) = find(&buf, "+ New group");
        assert_eq!(
            dialog.handle_click(col, row),
            DialogResult::Submit(Pick::Action)
        );
    }

    /// Each option owns one letter: the press selects and submits that option, and a letter
    /// no option claims leaves the dialog alone.
    #[test]
    fn a_mnemonic_letter_submits_its_option() {
        for (letter, expected) in [
            ('c', SortOrder::Custom),
            ('a', SortOrder::Name),
            ('n', SortOrder::Newest),
            ('C', SortOrder::Custom),
        ] {
            let mut dialog = SortPickerDialog::new(SortOrder::Name, None);
            assert_eq!(
                dialog.handle_key(key(KeyCode::Char(letter))),
                DialogResult::Submit(Pick::Option(expected)),
                "{letter}"
            );
        }
        for (letter, expected) in [('p', GroupByMode::Project), ('g', GroupByMode::Group)] {
            let mut dialog = GroupPickerDialog::new(GroupByMode::Group, None, true);
            assert_eq!(
                dialog.handle_key(key(KeyCode::Char(letter))),
                DialogResult::Submit(Pick::Option(expected)),
                "{letter}"
            );
        }

        let mut dialog = SortPickerDialog::new(SortOrder::Name, None);
        let before = dialog.selected;
        assert_eq!(
            dialog.handle_key(key(KeyCode::Char('q'))),
            DialogResult::Continue
        );
        assert_eq!(dialog.selected, before);
    }
}
