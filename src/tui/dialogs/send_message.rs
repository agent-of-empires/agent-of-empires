//! Send message dialog with multi-line text area

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::prelude::*;
use ratatui::widgets::*;
use ratatui_textarea::TextArea;

use super::DialogResult;
use crate::tui::responsive;
use crate::tui::styles::Theme;

pub struct SendMessageDialog {
    session_title: String,
    text_area: TextArea<'static>,
    /// Set for one keystroke after a kill that actually wrote to the yank
    /// buffer, while the footer offers Ctrl+P to paste it back.
    restore_armed: bool,
}

impl SendMessageDialog {
    pub fn new(session_title: &str) -> Self {
        let mut text_area = TextArea::new(vec![String::new()]);
        text_area.set_cursor_line_style(Style::default());

        Self {
            session_title: session_title.to_string(),
            text_area,
            restore_armed: false,
        }
    }

    fn get_text(&self) -> String {
        // Dictation pastes embedded lone CRs, which the textarea preserves and
        // an agent reads as a premature submit.
        let joined = self.text_area.lines().join("\n");
        joined.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// Run a kill, arming the restore hint only when it wrote to the yank
    /// buffer. A kill at a line edge joins lines via `delete_newline` without
    /// touching yank, so Ctrl+P would paste nothing or something stale.
    fn arm_if_yank_changed(&mut self, kill: impl FnOnce(&mut TextArea<'static>) -> bool) {
        let before = self.text_area.yank_text();
        let killed = kill(&mut self.text_area);
        self.restore_armed = killed && self.text_area.yank_text() != before;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<String> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Ctrl+P restores the last kill only while armed, else it falls
        // through to the textarea's cursor-up. Either case, Shift may be held.
        if ctrl && matches!(key.code, KeyCode::Char('p' | 'P')) && self.restore_armed {
            self.text_area.paste();
            self.restore_armed = false;
            return DialogResult::Continue;
        }

        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            KeyCode::Char('u' | 'U') if ctrl => {
                self.arm_if_yank_changed(|ta| ta.delete_line_by_head());
                DialogResult::Continue
            }
            // The textarea has Ctrl+K already; intercept it to arm the hint.
            KeyCode::Char('k' | 'K') if ctrl => {
                self.arm_if_yank_changed(|ta| ta.delete_line_by_end());
                DialogResult::Continue
            }
            // The textarea's other word-delete bindings (Alt+H, Alt+D,
            // Alt+Delete) fall through without arming the hint.
            KeyCode::Char('w' | 'W') if ctrl => {
                self.arm_if_yank_changed(|ta| ta.delete_word());
                DialogResult::Continue
            }
            KeyCode::Backspace if alt => {
                self.arm_if_yank_changed(|ta| ta.delete_word());
                DialogResult::Continue
            }
            // Most terminals send Shift+Enter as ESC + CR, which crossterm
            // decodes as Alt+Enter, so both modifiers insert a newline.
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.restore_armed = false;
                self.text_area.insert_newline();
                DialogResult::Continue
            }
            // Crossterm decodes a bare line feed as Ctrl+J, which some
            // terminals send for Shift+Enter. Without this arm it reaches the
            // textarea's delete-to-line-head and wipes the input.
            KeyCode::Char('j') if ctrl => {
                self.restore_armed = false;
                self.text_area.insert_newline();
                DialogResult::Continue
            }
            KeyCode::Enter => {
                let value = self.get_text().trim().to_string();
                if value.is_empty() {
                    DialogResult::Cancel
                } else {
                    DialogResult::Submit(value)
                }
            }
            _ => {
                self.restore_armed = false;
                self.text_area.input(key);
                DialogResult::Continue
            }
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.restore_armed = false;
        self.text_area.insert_str(text);
    }

    pub fn render(&self, frame: &mut Frame, area: Rect, theme: &Theme) {
        // 2 for borders + 1 per content line, min 3 (single line), max 12,
        // capped to viewport so the popover never paints under the iOS soft
        // keyboard if Event::Resize lands mid-render.
        let content_lines = self.text_area.lines().len() as u16;
        let height = (content_lines + 2).clamp(3, 12).min(area.height.max(3));
        let dialog_width = responsive::dialog_width(area.width);
        let dialog_area = super::centered_rect(area, dialog_width, height);

        frame.render_widget(Clear, dialog_area);

        let mut block = super::toned_dialog_block(
            format!(" > {} ", self.session_title),
            theme.accent,
            theme.accent,
        )
        .title_bottom(
            Line::from(vec![
                Span::styled(" Enter", Style::default().fg(theme.accent)),
                Span::styled(" send ", Style::default().fg(theme.dimmed)),
                Span::styled("Esc", Style::default().fg(theme.accent)),
                Span::styled(" cancel ", Style::default().fg(theme.dimmed)),
            ])
            .right_aligned(),
        );

        if self.restore_armed {
            block = block.title_bottom(
                Line::from(vec![
                    Span::styled(" Ctrl+P", Style::default().fg(theme.accent)),
                    Span::styled(" restore deleted text ", Style::default().fg(theme.dimmed)),
                ])
                .left_aligned(),
            );
        }

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let mut text_area_clone = self.text_area.clone();
        text_area_clone.set_style(Style::default().fg(theme.text));
        text_area_clone.set_cursor_style(Style::default().fg(theme.background).bg(theme.accent));

        frame.render_widget(&text_area_clone, inner);

        if inner.width > 0 && inner.height > 0 {
            let cursor = text_area_clone.screen_cursor();
            let max_x = inner.x.saturating_add(inner.width.saturating_sub(1));
            let max_y = inner.y.saturating_add(inner.height.saturating_sub(1));
            let cursor_x = inner.x.saturating_add(cursor.col as u16).min(max_x);
            let cursor_y = inner.y.saturating_add(cursor.row as u16).min(max_y);
            frame.set_cursor_position(Position::new(cursor_x, cursor_y));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_keys::{alt_key, ctrl_key, key, shift_key};

    fn dialog() -> SendMessageDialog {
        SendMessageDialog::new("Test Session")
    }

    fn type_str(dialog: &mut SendMessageDialog, text: &str) {
        for c in text.chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
    }

    fn render_cursor_position(dialog: &SendMessageDialog, width: u16, height: u16) -> Position {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let theme = load_theme("empire");
        terminal
            .draw(|f| dialog.render(f, f.area(), &theme))
            .unwrap();
        terminal.backend_mut().get_cursor_position().unwrap()
    }

    #[test]
    fn enter_submits_typed_text_and_cancels_when_empty() {
        let mut d = dialog();
        assert!(matches!(
            d.handle_key(key(KeyCode::Enter)),
            DialogResult::Cancel
        ));
        assert!(matches!(
            d.handle_key(key(KeyCode::Esc)),
            DialogResult::Cancel
        ));

        let mut d = dialog();
        assert!(matches!(
            d.handle_key(key(KeyCode::Char('h'))),
            DialogResult::Continue
        ));
        d.handle_key(key(KeyCode::Char('i')));
        assert!(
            matches!(d.handle_key(key(KeyCode::Enter)), DialogResult::Submit(ref s) if s == "hi")
        );
    }

    #[test]
    fn every_newline_chord_inserts_a_line_break() {
        // Terminals send Shift+Enter as Alt+Enter or as a bare line feed,
        // which crossterm decodes as Ctrl+J. All three must insert a newline
        // rather than reach the textarea's delete-to-line-head default.
        for newline in [
            shift_key(KeyCode::Enter),
            alt_key(KeyCode::Enter),
            ctrl_key(KeyCode::Char('j')),
        ] {
            let mut d = dialog();
            type_str(&mut d, "l1");
            assert!(matches!(d.handle_key(newline), DialogResult::Continue));
            type_str(&mut d, "l2");
            assert_eq!(d.get_text(), "l1\nl2");
            assert!(
                matches!(d.handle_key(key(KeyCode::Enter)), DialogResult::Submit(ref s) if s == "l1\nl2")
            );
        }
    }

    #[test]
    fn paste_inserts_at_the_cursor_and_normalizes_carriage_returns() {
        let mut d = dialog();
        type_str(&mut d, "hi ");
        d.handle_paste("world");
        assert_eq!(d.get_text(), "hi world");
        assert!(
            matches!(d.handle_key(key(KeyCode::Enter)), DialogResult::Submit(ref s) if s == "hi world")
        );

        // Dictation emits lone CRs as sentence breaks; embedded \r reaches the
        // agent as a premature submit, so both forms collapse to \n.
        for (pasted, want) in [
            ("line1\nline2\nline3", "line1\nline2\nline3"),
            (
                "first\r\nsecond\rthird\r\nfourth",
                "first\nsecond\nthird\nfourth",
            ),
        ] {
            let mut d = dialog();
            d.handle_paste(pasted);
            assert_eq!(d.get_text(), want);
        }
    }

    #[test]
    fn kills_arm_the_restore_and_ctrl_p_pastes_the_yank_back() {
        // (typed text, keys before the kill, kill chord, text left, restored)
        type Setup = fn(&mut SendMessageDialog);
        let cases: &[(&str, Setup, KeyEvent, &str, &str)] = &[
            ("hi", |_| {}, ctrl_key(KeyCode::Char('u')), "", "hi"),
            (
                "abc",
                |d| {
                    d.handle_key(key(KeyCode::Left));
                },
                ctrl_key(KeyCode::Char('u')),
                "c",
                "abc",
            ),
            (
                "abc",
                |d| {
                    d.handle_key(key(KeyCode::Home));
                },
                ctrl_key(KeyCode::Char('k')),
                "",
                "abc",
            ),
            (
                "abc",
                |d| {
                    d.handle_key(key(KeyCode::Left));
                },
                ctrl_key(KeyCode::Char('k')),
                "ab",
                "abc",
            ),
            (
                "hello world",
                |_| {},
                ctrl_key(KeyCode::Char('w')),
                "hello ",
                "hello world",
            ),
            (
                "foo bar",
                |_| {},
                KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT),
                "foo ",
                "foo bar",
            ),
        ];
        for (text, before, kill, left, restored) in cases {
            let mut d = dialog();
            type_str(&mut d, text);
            before(&mut d);
            assert!(matches!(d.handle_key(*kill), DialogResult::Continue));
            assert_eq!(d.get_text(), *left, "{text}");
            assert!(d.restore_armed, "{text}");
            d.handle_key(ctrl_key(KeyCode::Char('p')));
            assert_eq!(d.get_text(), *restored, "{text}");
            assert!(!d.restore_armed, "{text}");
        }

        // Ctrl+U on a later line kills only that line's prefix.
        let mut d = dialog();
        type_str(&mut d, "a");
        d.handle_key(shift_key(KeyCode::Enter));
        type_str(&mut d, "bcd");
        d.handle_key(key(KeyCode::Left));
        d.handle_key(ctrl_key(KeyCode::Char('u')));
        assert_eq!(d.get_text(), "a\nd");
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(d.get_text(), "a\nbcd");
    }

    #[test]
    fn a_kill_that_never_touched_the_yank_buffer_leaves_the_restore_disarmed() {
        // At a line edge the kill joins lines via `delete_newline` without
        // writing to yank, so arming would make Ctrl+P paste stale content.
        let mut d = dialog();
        d.handle_key(ctrl_key(KeyCode::Char('u')));
        assert!(!d.restore_armed, "nothing to kill");

        let mut d = dialog();
        type_str(&mut d, "a");
        d.handle_key(ctrl_key(KeyCode::Char('k')));
        assert_eq!(d.get_text(), "a");
        assert!(!d.restore_armed, "end of the only line");

        // (keys that park the cursor on the joining edge, kill chord)
        type Park = fn(&mut SendMessageDialog);
        let joins: &[(Park, KeyEvent)] = &[
            (
                |d| {
                    d.handle_key(key(KeyCode::Home));
                },
                ctrl_key(KeyCode::Char('u')),
            ),
            (
                |d| {
                    d.handle_key(key(KeyCode::Up));
                    d.handle_key(key(KeyCode::End));
                },
                ctrl_key(KeyCode::Char('k')),
            ),
        ];
        for (park, kill) in joins {
            let mut d = dialog();
            type_str(&mut d, "a");
            d.handle_key(shift_key(KeyCode::Enter));
            type_str(&mut d, "b");
            park(&mut d);
            d.handle_key(*kill);
            assert_eq!(d.get_text(), "ab");
            assert!(!d.restore_armed);
        }

        // Unarmed, Ctrl+P falls through to the textarea's cursor-up without
        // corrupting the text.
        let mut d = dialog();
        type_str(&mut d, "h");
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(d.get_text(), "h");
        assert!(!d.restore_armed);
    }

    #[test]
    fn typing_or_pasting_disarms_the_restore() {
        let mut d = dialog();
        type_str(&mut d, "x");
        d.handle_key(ctrl_key(KeyCode::Char('u')));
        assert!(d.restore_armed);
        type_str(&mut d, "y");
        assert!(!d.restore_armed);
        d.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(d.get_text(), "y");

        let mut d = dialog();
        type_str(&mut d, "x");
        d.handle_key(ctrl_key(KeyCode::Char('u')));
        d.handle_paste("pasted");
        assert!(!d.restore_armed);
        assert_eq!(d.get_text(), "pasted");
    }

    #[test]
    fn shifted_kill_and_restore_chords_still_work() {
        // Some terminals deliver Ctrl+Shift+U as Char('U') with CONTROL.
        let shift_ctrl = |c| {
            KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            )
        };
        let mut d = dialog();
        type_str(&mut d, "z");
        d.handle_key(shift_ctrl('U'));
        assert_eq!(d.get_text(), "");
        assert!(d.restore_armed);
        d.handle_key(shift_ctrl('P'));
        assert_eq!(d.get_text(), "z");
    }

    #[test]
    fn render_puts_the_terminal_cursor_on_the_display_column() {
        // An 80-column viewport gives a 64-wide dialog at x=8 (inner x=9) and
        // 3 rows at y=10 (inner y=11). A wide char advances two cells, so both
        // inputs leave the cursor in the same place.
        for text in ["hi", "你"] {
            let mut d = dialog();
            type_str(&mut d, text);
            assert_eq!(render_cursor_position(&d, 80, 24), Position::new(11, 11));
        }
    }
}
