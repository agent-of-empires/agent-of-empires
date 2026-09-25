//! Custom instruction editor.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;
use ratatui_textarea::TextArea;

use super::DialogResult;
use crate::tui::styles::Theme;

pub struct CustomInstructionDialog {
    focused_zone: usize,   // 0 = text area, 1 = button row
    focused_button: usize, // 0 = Save, 1 = Cancel
    text_area: TextArea<'static>,
}

impl CustomInstructionDialog {
    pub fn new(current_value: Option<String>) -> Self {
        let text = current_value.clone().unwrap_or_default();
        let lines: Vec<String> = if text.is_empty() {
            vec![String::new()]
        } else {
            text.lines().map(|l| l.to_string()).collect()
        };

        let mut text_area = TextArea::new(lines);
        text_area.set_cursor_line_style(Style::default());

        Self {
            focused_zone: 0,
            focused_button: 0,
            text_area,
        }
    }

    fn get_text(&self) -> String {
        self.text_area.lines().join("\n")
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<Option<String>> {
        match key.code {
            KeyCode::Esc => DialogResult::Cancel,

            KeyCode::Tab | KeyCode::BackTab => {
                self.focused_zone = if self.focused_zone == 0 { 1 } else { 0 };
                DialogResult::Continue
            }

            KeyCode::Enter if self.focused_zone == 1 => {
                if self.focused_button == 0 {
                    let text = self.get_text();
                    let value = if text.trim().is_empty() {
                        None
                    } else {
                        Some(text)
                    };
                    DialogResult::Submit(value)
                } else {
                    DialogResult::Cancel
                }
            }

            KeyCode::Left if self.focused_zone == 1 => {
                self.focused_button = 0;
                DialogResult::Continue
            }
            KeyCode::Right if self.focused_zone == 1 => {
                self.focused_button = 1;
                DialogResult::Continue
            }

            _ if self.focused_zone == 0 => {
                self.text_area.input(key);
                DialogResult::Continue
            }

            _ => DialogResult::Continue,
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.focused_zone == 0 {
            self.text_area.insert_str(text);
        }
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_width = (area.width * 70 / 100).max(40).min(area.width);
        let dialog_height = (area.height * 60 / 100).max(10).min(area.height);
        let block = super::dialog_block(" Edit Custom Instruction ", theme);
        let (_, inner) =
            super::render_dialog_frame(frame, area, dialog_width, dialog_height, block);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Min(3),    // Text area
                Constraint::Length(3), // Button row
                Constraint::Length(1), // Hint bar
            ])
            .split(inner);

        let textarea_border_color = if self.focused_zone == 0 {
            theme.accent
        } else {
            theme.border
        };
        let textarea_block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(textarea_border_color));
        let textarea_inner = textarea_block.inner(chunks[0]);

        let mut text_area_clone = self.text_area.clone();
        text_area_clone.set_block(textarea_block);
        text_area_clone.set_style(Style::default().fg(theme.text));
        if self.focused_zone == 0 {
            text_area_clone
                .set_cursor_style(Style::default().fg(theme.background).bg(theme.accent));
        } else {
            text_area_clone.set_cursor_style(Style::default());
        }

        frame.render_widget(&text_area_clone, chunks[0]);
        if self.focused_zone == 0 && textarea_inner.width > 0 && textarea_inner.height > 0 {
            let cursor = text_area_clone.screen_cursor();
            let max_x = textarea_inner
                .x
                .saturating_add(textarea_inner.width.saturating_sub(1));
            let max_y = textarea_inner
                .y
                .saturating_add(textarea_inner.height.saturating_sub(1));
            let cursor_x = textarea_inner
                .x
                .saturating_add(cursor.col as u16)
                .min(max_x);
            let cursor_y = textarea_inner
                .y
                .saturating_add(cursor.row as u16)
                .min(max_y);
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
        }

        let button_area = chunks[1];
        let button_layout = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Fill(1),
                Constraint::Length(10),
                Constraint::Length(2),
                Constraint::Length(10),
                Constraint::Fill(1),
            ])
            .split(Rect {
                x: button_area.x,
                y: button_area.y + 1,
                width: button_area.width,
                height: 1,
            });

        let save_style = if self.focused_zone == 1 && self.focused_button == 0 {
            Style::default()
                .fg(theme.background)
                .bg(theme.accent)
                .bold()
        } else {
            Style::default().fg(theme.text)
        };

        let cancel_style = if self.focused_zone == 1 && self.focused_button == 1 {
            Style::default().fg(theme.background).bg(theme.error).bold()
        } else {
            Style::default().fg(theme.text)
        };

        frame.render_widget(
            Paragraph::new("  Save  ")
                .style(save_style)
                .alignment(Alignment::Center),
            button_layout[1],
        );
        frame.render_widget(
            Paragraph::new(" Cancel ")
                .style(cancel_style)
                .alignment(Alignment::Center),
            button_layout[3],
        );

        let hint = Line::from(vec![
            Span::styled("Tab", Style::default().fg(theme.hint)),
            Span::raw(" switch focus  "),
            Span::styled("Enter", Style::default().fg(theme.hint)),
            Span::raw(" edit/confirm  "),
            Span::styled("Esc", Style::default().fg(theme.hint)),
            Span::raw(" cancel"),
        ]);
        frame.render_widget(Paragraph::new(hint), chunks[2]);
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

    #[test]
    fn new_prepopulates_and_tab_toggles_zones() {
        for (initial, text) in [
            (Some("hello world"), "hello world"),
            (None, ""),
            (Some("line1\nline2\nline3"), "line1\nline2\nline3"),
        ] {
            let dialog = CustomInstructionDialog::new(initial.map(str::to_string));
            assert_eq!(dialog.get_text(), text);
            assert_eq!((dialog.focused_zone, dialog.focused_button), (0, 0));
        }
        for tab in [key(KeyCode::Tab), shift_key(KeyCode::Tab)] {
            let mut dialog = CustomInstructionDialog::new(None);
            dialog.handle_key(tab);
            assert_eq!(dialog.focused_zone, 1);
            dialog.handle_key(tab);
            assert_eq!(dialog.focused_zone, 0);
        }
        let mut dialog = CustomInstructionDialog::new(None);
        dialog.focused_zone = 1;
        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.focused_button, 1);
        dialog.handle_key(key(KeyCode::Left));
        assert_eq!(dialog.focused_button, 0);
    }

    #[test]
    fn key_outcomes() {
        let submit = |t: &str| DialogResult::Submit(Some(t.to_string()));
        // (initial text, zone, button, key, outcome). Blank text submits None.
        let cases = [
            (None, 0, 0, KeyCode::Esc, DialogResult::Cancel),
            (None, 1, 0, KeyCode::Esc, DialogResult::Cancel),
            (None, 0, 0, KeyCode::Enter, DialogResult::Continue),
            (Some("test text"), 1, 0, KeyCode::Enter, submit("test text")),
            (
                Some("test text"),
                1,
                1,
                KeyCode::Enter,
                DialogResult::Cancel,
            ),
            (None, 1, 0, KeyCode::Enter, DialogResult::Submit(None)),
            (
                Some("   \n  "),
                1,
                0,
                KeyCode::Enter,
                DialogResult::Submit(None),
            ),
        ];
        for (initial, zone, button, code, want) in cases {
            let mut dialog = CustomInstructionDialog::new(initial.map(str::to_string));
            dialog.focused_zone = zone;
            dialog.focused_button = button;
            assert_eq!(
                dialog.handle_key(key(code)),
                want,
                "{initial:?} zone {zone} button {button} {code:?}"
            );
        }
    }
}
