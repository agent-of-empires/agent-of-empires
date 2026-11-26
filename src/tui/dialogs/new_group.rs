//! New group: a name plus the Sessions that move into it, so a Group is never created empty.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use tui_input::backend::crossterm::EventHandler;
use tui_input::Input;

use super::DialogResult;
use crate::tui::components::hint_buttons::HintButtons;
use crate::tui::components::render_text_field;
use crate::tui::styles::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewGroupData {
    pub profile: String,
    pub path: String,
    pub sessions: Vec<String>,
}

pub struct NewGroupDialog {
    profile: String,
    name: Input,
    /// `(id, title)` of the Sessions that may join, in list order.
    candidates: Vec<(String, String)>,
    checked: Vec<bool>,
    /// Group paths that already exist in the profile.
    existing: Vec<String>,
    list_focused: bool,
    cursor: usize,
    scroll: usize,
    error: Option<&'static str>,
    name_area: Rect,
    list_area: Rect,
    footer: HintButtons,
}

const LIST_ROWS: u16 = 8;

impl NewGroupDialog {
    /// `preselected` is checked when it is among the candidates.
    pub fn new(
        profile: String,
        candidates: Vec<(String, String)>,
        existing: Vec<String>,
        preselected: Option<&str>,
    ) -> Self {
        let checked: Vec<bool> = candidates
            .iter()
            .map(|(id, _)| Some(id.as_str()) == preselected)
            .collect();
        let cursor = checked.iter().position(|c| *c).unwrap_or(0);
        Self {
            profile,
            name: Input::default(),
            candidates,
            checked,
            existing,
            list_focused: false,
            cursor,
            scroll: 0,
            error: None,
            name_area: Rect::default(),
            list_area: Rect::default(),
            footer: HintButtons::default(),
        }
    }

    fn submit(&mut self) -> DialogResult<NewGroupData> {
        let path = self.name.value().trim().to_owned();
        self.error = if path.is_empty() {
            Some("Enter a group name.")
        } else if self.existing.contains(&path) {
            Some("A group with this name already exists.")
        } else if !self.checked.contains(&true) {
            Some("Pick at least one session.")
        } else {
            None
        };
        if self.error.is_some() {
            return DialogResult::Continue;
        }
        let sessions = self
            .candidates
            .iter()
            .zip(&self.checked)
            .filter(|(_, checked)| **checked)
            .map(|((id, _), _)| id.clone())
            .collect();
        DialogResult::Submit(NewGroupData {
            profile: self.profile.clone(),
            path,
            sessions,
        })
    }

    fn toggle(&mut self, idx: usize) {
        if let Some(checked) = self.checked.get_mut(idx) {
            *checked = !*checked;
            self.error = None;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<NewGroupData> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Enter => self.submit(),
            KeyCode::Tab | KeyCode::BackTab => {
                self.list_focused = !self.list_focused && !self.candidates.is_empty();
                DialogResult::Continue
            }
            KeyCode::Down if !self.list_focused => {
                self.list_focused = !self.candidates.is_empty();
                DialogResult::Continue
            }
            KeyCode::Up if self.list_focused && self.cursor == 0 => {
                self.list_focused = false;
                DialogResult::Continue
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End if self.list_focused => {
                super::navigate_list(&mut self.cursor, self.candidates.len(), key.code);
                DialogResult::Continue
            }
            KeyCode::Char(' ') if self.list_focused => {
                self.toggle(self.cursor);
                DialogResult::Continue
            }
            _ if !self.list_focused => {
                self.name.handle_event(&crossterm::event::Event::Key(key));
                self.error = None;
                DialogResult::Continue
            }
            _ => DialogResult::Continue,
        }
    }

    /// A click on the name focuses it and one on a Session toggles it; a click on a footer
    /// hint returns the key it stands for, for the caller to press.
    pub fn handle_click(&mut self, col: u16, row: u16) -> Option<KeyEvent> {
        let pos = Position::from((col, row));
        if self.name_area.contains(pos) {
            self.list_focused = false;
            return None;
        }
        if let Some(line) = super::row_index(self.list_area, col, row, LIST_ROWS as usize) {
            let idx = self.scroll + line;
            if idx < self.candidates.len() {
                self.list_focused = true;
                self.cursor = idx;
                self.toggle(idx);
            }
            return None;
        }
        self.footer.key_at(col, row)
    }

    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.footer.handle_hover(col, row)
    }

    pub fn handle_paste(&mut self, text: &str) {
        if !self.list_focused {
            super::paste_into_input(&mut self.name, text);
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let block = super::dialog_block(" New Group ", theme);
        let (_, inner) = super::render_dialog_frame(frame, area, 60, LIST_ROWS + 10, block);
        let [name, _, label, list, error, hint] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(LIST_ROWS),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .margin(1)
        .areas(inner);
        self.name_area = name;
        self.list_area = list;
        render_text_field(
            frame,
            name,
            "Name:",
            &self.name,
            !self.list_focused,
            Some("work/api"),
            theme,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Sessions to move in (at least one):",
                Style::default().fg(theme.dimmed),
            )),
            label,
        );

        let rows = LIST_ROWS as usize;
        if self.cursor < self.scroll {
            self.scroll = self.cursor;
        } else if self.cursor >= self.scroll + rows {
            self.scroll = self.cursor + 1 - rows;
        }
        let lines: Vec<Line> = if self.candidates.is_empty() {
            vec![Line::styled(
                "No session in this profile can move.",
                Style::default().fg(theme.dimmed),
            )]
        } else {
            self.candidates
                .iter()
                .zip(&self.checked)
                .enumerate()
                .skip(self.scroll)
                .take(rows)
                .map(|(i, ((_, title), checked))| {
                    let style = if self.list_focused && i == self.cursor {
                        Style::default().fg(theme.accent).bold()
                    } else {
                        Style::default().fg(theme.text)
                    };
                    let mark = if *checked { "[x] " } else { "[ ] " };
                    Line::from(vec![
                        Span::styled(mark, style),
                        Span::styled(title.as_str(), style),
                    ])
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), list);
        if let Some(message) = self.error {
            frame.render_widget(
                Paragraph::new(Span::styled(message, Style::default().fg(theme.error))),
                error,
            );
        }
        self.footer.render(
            frame,
            hint,
            theme,
            &[
                ("Tab", "switch", KeyCode::Tab),
                ("Space", "check", KeyCode::Null),
                ("Enter", "create", KeyCode::Enter),
                ("Esc", "cancel", KeyCode::Esc),
            ],
            Alignment::Left,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::key;
    use KeyCode::{Char, Down, Enter, Tab, Up};

    fn dialog(preselected: Option<&str>) -> NewGroupDialog {
        NewGroupDialog::new(
            "default".into(),
            vec![
                ("a".into(), "Aztecs".into()),
                ("b".into(), "Britons".into()),
            ],
            vec!["work".into()],
            preselected,
        )
    }

    /// Enter creates only with a new name and at least one Session, the selected one checked
    /// from the start; otherwise it stays open and says what is missing.
    #[test]
    fn submit_needs_a_new_name_and_a_session() {
        let cases: [(Option<&str>, &[KeyCode], Result<&[&str], &str>); 6] = [
            (Some("b"), &[Char('x')], Ok(&["b"])),
            (Some("b"), &[], Err("Enter a group name.")),
            (
                Some("b"),
                &[Char('w'), Char('o'), Char('r'), Char('k')],
                Err("A group with this name already exists."),
            ),
            (None, &[Char('x')], Err("Pick at least one session.")),
            (
                None,
                &[Char('x'), Tab, Char(' '), Down, Char(' ')],
                Ok(&["a", "b"]),
            ),
            // Space types into the name; Up from the first Session returns to it.
            (
                Some("a"),
                &[Char('a'), Down, Up, Char(' '), Char('b')],
                Ok(&["a"]),
            ),
        ];
        for (preselected, keys, expected) in cases {
            let mut d = dialog(preselected);
            for code in keys {
                d.handle_key(key(*code));
            }
            match (d.handle_key(key(Enter)), expected) {
                (DialogResult::Submit(data), Ok(ids)) => {
                    assert_eq!(data.sessions, ids, "{keys:?}");
                    assert_eq!(data.profile, "default");
                    assert!(!data.path.is_empty());
                }
                (DialogResult::Continue, Err(message)) => {
                    assert_eq!(d.error, Some(message), "{keys:?}")
                }
                (_, expected) => panic!("{keys:?}: expected {expected:?}"),
            }
        }
    }
}
