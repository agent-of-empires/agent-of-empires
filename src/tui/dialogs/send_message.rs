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
    /// True for one keystroke after a kill (Ctrl+U/K/W or Alt+Backspace) that
    /// actually wrote to the textarea's yank buffer. While true, the footer
    /// shows the "Ctrl+P restore deleted text" hint and Ctrl+P pastes the yank
    /// back. Any other key clears it.
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
        // ratatui_textarea preserves embedded CRs from voice/dictation paste
        // (iOS speech often emits lone \r as a sentence break). Sending raw \r
        // through to claude-code causes the agent to submit prematurely or
        // receive garbled input — normalize before submit.
        let joined = self.text_area.lines().join("\n");
        joined.replace("\r\n", "\n").replace('\r', "\n")
    }

    /// Run a kill operation and arm the restore hint only if it actually wrote
    /// to the textarea's yank buffer. Some "kills" (e.g. Ctrl+U at column 0,
    /// Ctrl+K at end-of-line) call `delete_newline` under the hood, which joins
    /// lines without touching yank, so a subsequent Ctrl+P paste would either
    /// do nothing or splat stale content.
    fn arm_if_yank_changed(&mut self, kill: impl FnOnce(&mut TextArea<'static>) -> bool) {
        let before = self.text_area.yank_text();
        let killed = kill(&mut self.text_area);
        self.restore_armed = killed && self.text_area.yank_text() != before;
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<String> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // Ctrl+P restores the last kill (from Ctrl+U/K/W) only while armed.
        // Otherwise it falls through to the textarea's default (cursor up).
        // Match both lowercase and uppercase in case the terminal sends 'P'
        // when Shift was also held.
        if ctrl && matches!(key.code, KeyCode::Char('p' | 'P')) && self.restore_armed {
            self.text_area.paste();
            self.restore_armed = false;
            return DialogResult::Continue;
        }

        match key.code {
            KeyCode::Esc => DialogResult::Cancel,
            // Ctrl+U: delete from cursor to start of line. The deleted slice goes
            // into the textarea's yank buffer; Ctrl+P pastes it back.
            KeyCode::Char('u' | 'U') if ctrl => {
                self.arm_if_yank_changed(|ta| ta.delete_line_by_head());
                DialogResult::Continue
            }
            // Ctrl+K: delete from cursor to end of line. The textarea has this
            // by default, but we intercept so we can arm the restore hint.
            KeyCode::Char('k' | 'K') if ctrl => {
                self.arm_if_yank_changed(|ta| ta.delete_line_by_end());
                DialogResult::Continue
            }
            // Ctrl+W: delete previous word. Same yank buffer as Ctrl+U/K, so
            // we arm the hint for symmetry. Note: ratatui-textarea also binds
            // Alt+H, Alt+Backspace, Alt+D, Alt+Delete to word-delete, but those
            // are less commonly typed and fall through to the textarea's native
            // handler without arming the hint.
            KeyCode::Char('w' | 'W') if ctrl => {
                self.arm_if_yank_changed(|ta| ta.delete_word());
                DialogResult::Continue
            }
            // Alt+Backspace: macOS-style word-backspace. Mirrors Ctrl+W.
            KeyCode::Backspace if alt => {
                self.arm_if_yank_changed(|ta| ta.delete_word());
                DialogResult::Continue
            }
            // Shift+Enter inserts a newline.
            // Most terminals send Shift+Enter as ESC + CR (\x1b\r), which crossterm
            // decodes as Alt+Enter, so we accept both ALT and SHIFT modifiers.
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.restore_armed = false;
                self.text_area.insert_newline();
                DialogResult::Continue
            }
            // Ctrl+J is how crossterm decodes a bare line feed (\n, 0x0A) once
            // raw mode is on; some terminals send that for Shift+Enter (e.g. a
            // Ghostty `keybind = shift+enter=text:\n`). Treat it as a newline
            // like the rest of the Enter family. Without this it falls through
            // to the textarea, whose default binds Ctrl+J to delete-to-line-head
            // and silently wipes the input.
            KeyCode::Char('j') if ctrl => {
                self.restore_armed = false;
                self.text_area.insert_newline();
                DialogResult::Continue
            }
            // Plain Enter sends
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

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.accent))
            .title(format!(" > {} ", self.session_title))
            .title_style(Style::default().fg(theme.accent).bold())
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn shift_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::SHIFT)
    }

    fn alt_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::ALT)
    }

    fn ctrl_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::CONTROL)
    }

    fn render_cursor_position(dialog: &SendMessageDialog, width: u16, height: u16) -> Position {
        use crate::tui::styles::load_theme;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;

        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let theme = load_theme("empire");
        terminal
            .draw(|f| {
                let area = f.area();
                dialog.render(f, area, &theme);
            })
            .unwrap();
        terminal.backend_mut().get_cursor_position().unwrap()
    }

    #[test]
    fn test_esc_cancels() {
        let mut dialog = SendMessageDialog::new("Test Session");
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_on_empty_cancels() {
        let mut dialog = SendMessageDialog::new("Test Session");
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_with_text_submits() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('h')));
        dialog.handle_key(key(KeyCode::Char('i')));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(ref s) if s == "hi"));
    }

    #[test]
    fn test_typing_continues() {
        let mut dialog = SendMessageDialog::new("Test Session");
        let result = dialog.handle_key(key(KeyCode::Char('a')));
        assert!(matches!(result, DialogResult::Continue));
    }

    #[test]
    fn render_places_terminal_cursor_at_textarea_cursor() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('h')));
        dialog.handle_key(key(KeyCode::Char('i')));

        // 80-col viewport -> 64-col dialog centered at x=8, inner x=9.
        // 3-row dialog centered at y=10, inner y=11. Cursor is after "hi".
        assert_eq!(
            render_cursor_position(&dialog, 80, 24),
            Position::new(11, 11)
        );
    }

    #[test]
    fn render_cursor_uses_display_columns_for_wide_chars() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('你')));

        // The visual cursor should move two cells for an East Asian wide char,
        // not one Unicode scalar position.
        assert_eq!(
            render_cursor_position(&dialog, 80, 24),
            Position::new(11, 11)
        );
    }

    #[test]
    fn test_shift_enter_adds_newline() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        let result = dialog.handle_key(shift_key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Continue));
        dialog.handle_key(key(KeyCode::Char('b')));
        assert_eq!(dialog.get_text(), "a\nb");
    }

    #[test]
    fn test_ctrl_j_adds_newline() {
        // crossterm decodes a bare line feed (\n, 0x0A) as Ctrl+J in raw mode;
        // some terminals send that for Shift+Enter (e.g. Ghostty
        // `keybind = shift+enter=text:\n`). It must insert a newline, not fall
        // through to the textarea's delete-to-line-head default.
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        let result = dialog.handle_key(ctrl_key(KeyCode::Char('j')));
        assert!(matches!(result, DialogResult::Continue));
        dialog.handle_key(key(KeyCode::Char('b')));
        assert_eq!(dialog.get_text(), "a\nb");
    }

    #[test]
    fn test_alt_enter_adds_newline() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        let result = dialog.handle_key(alt_key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Continue));
        dialog.handle_key(key(KeyCode::Char('b')));
        assert_eq!(dialog.get_text(), "a\nb");
    }

    #[test]
    fn test_multiline_submit() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('l')));
        dialog.handle_key(key(KeyCode::Char('1')));
        dialog.handle_key(shift_key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Char('l')));
        dialog.handle_key(key(KeyCode::Char('2')));
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(ref s) if s == "l1\nl2"));
    }

    #[test]
    fn test_paste_single_line() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_paste("hello world");
        assert_eq!(dialog.get_text(), "hello world");
    }

    #[test]
    fn test_paste_multiline() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_paste("line1\nline2\nline3");
        assert_eq!(dialog.get_text(), "line1\nline2\nline3");
    }

    #[test]
    fn test_paste_then_submit() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_paste("pasted text");
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Submit(ref s) if s == "pasted text"));
    }

    #[test]
    fn test_paste_appends_to_existing() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('h')));
        dialog.handle_key(key(KeyCode::Char('i')));
        dialog.handle_key(key(KeyCode::Char(' ')));
        dialog.handle_paste("world");
        assert_eq!(dialog.get_text(), "hi world");
    }

    /// iOS Speech-to-Text emits lone CR as sentence breaks. Without normalization,
    /// `get_text` returned strings with embedded \r that caused premature submit
    /// or garbled input downstream. Verify both \r\n and lone \r collapse to \n
    /// regardless of which order they appear in.
    #[test]
    fn test_get_text_normalizes_carriage_returns() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_paste("first\r\nsecond\rthird\r\nfourth");
        assert_eq!(dialog.get_text(), "first\nsecond\nthird\nfourth");
    }

    #[test]
    fn test_get_text_preserves_plain_newlines() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_paste("a\nb\nc");
        assert_eq!(dialog.get_text(), "a\nb\nc");
    }

    #[test]
    fn test_ctrl_u_deletes_to_start_of_line() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('h')));
        dialog.handle_key(key(KeyCode::Char('i')));
        // Cursor sits at end after typing, so Ctrl+U kills the whole line.
        let result = dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert!(matches!(result, DialogResult::Continue));
        assert_eq!(dialog.get_text(), "");
        assert!(dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_u_partial_delete() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Char('c')));
        dialog.handle_key(key(KeyCode::Left));
        // Cursor between 'b' and 'c'. Ctrl+U deletes "ab".
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert_eq!(dialog.get_text(), "c");
        assert!(dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_k_deletes_to_end_of_line() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Char('c')));
        dialog.handle_key(key(KeyCode::Home));
        let result = dialog.handle_key(ctrl_key(KeyCode::Char('k')));
        assert!(matches!(result, DialogResult::Continue));
        assert_eq!(dialog.get_text(), "");
        assert!(dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_k_partial_delete() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Char('c')));
        dialog.handle_key(key(KeyCode::Left));
        // Cursor between 'b' and 'c'. Ctrl+K deletes "c".
        dialog.handle_key(ctrl_key(KeyCode::Char('k')));
        assert_eq!(dialog.get_text(), "ab");
        assert!(dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_u_on_empty_does_not_arm_restore() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_k_at_end_of_input_does_not_arm() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        // Cursor at end of single line, no newline after, so nothing to kill.
        dialog.handle_key(ctrl_key(KeyCode::Char('k')));
        assert_eq!(dialog.get_text(), "a");
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_p_restores_after_ctrl_u() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('h')));
        dialog.handle_key(key(KeyCode::Char('i')));
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert_eq!(dialog.get_text(), "");
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(dialog.get_text(), "hi");
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_p_restores_after_ctrl_k() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Home));
        dialog.handle_key(ctrl_key(KeyCode::Char('k')));
        assert_eq!(dialog.get_text(), "");
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(dialog.get_text(), "ab");
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_p_without_arm_passes_through() {
        // No kill has happened, so Ctrl+P should not be intercepted.
        // The textarea's default Ctrl+P (cursor up) takes over; on single-line
        // input that's effectively a no-op but it must not corrupt text/state.
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('h')));
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(dialog.get_text(), "h");
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_typing_disarms_restore() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('x')));
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert!(dialog.restore_armed);
        dialog.handle_key(key(KeyCode::Char('y')));
        assert!(!dialog.restore_armed);
        // Subsequent Ctrl+P no longer restores.
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(dialog.get_text(), "y");
    }

    #[test]
    fn test_paste_disarms_restore() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('x')));
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert!(dialog.restore_armed);
        dialog.handle_paste("pasted");
        assert!(!dialog.restore_armed);
        assert_eq!(dialog.get_text(), "pasted");
    }

    #[test]
    fn test_ctrl_w_deletes_previous_word() {
        let mut dialog = SendMessageDialog::new("Test Session");
        for c in "hello world".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(ctrl_key(KeyCode::Char('w')));
        assert_eq!(dialog.get_text(), "hello ");
        assert!(dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_p_restores_after_ctrl_w() {
        let mut dialog = SendMessageDialog::new("Test Session");
        for c in "hello world".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(ctrl_key(KeyCode::Char('w')));
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(dialog.get_text(), "hello world");
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_alt_backspace_deletes_previous_word() {
        let mut dialog = SendMessageDialog::new("Test Session");
        for c in "foo bar".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        dialog.handle_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::ALT));
        assert_eq!(dialog.get_text(), "foo ");
        assert!(dialog.restore_armed);
    }

    #[test]
    fn test_uppercase_ctrl_u_clears() {
        // Some terminals deliver Ctrl+Shift+U as Char('U') + CONTROL. Treat it
        // the same as lowercase Ctrl+U so the kill still works.
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('h')));
        dialog.handle_key(KeyEvent::new(
            KeyCode::Char('U'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(dialog.get_text(), "");
        assert!(dialog.restore_armed);
    }

    #[test]
    fn test_uppercase_ctrl_p_restores() {
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('z')));
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        dialog.handle_key(KeyEvent::new(
            KeyCode::Char('P'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(dialog.get_text(), "z");
    }

    #[test]
    fn test_ctrl_u_at_start_of_line_joins_without_arming() {
        // Cursor at column 0 of line 2: Ctrl+U deletes the newline (joins
        // lines) but does NOT touch the yank buffer, so we should not arm the
        // restore hint - otherwise Ctrl+P would paste empty/stale content.
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(shift_key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Home));
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert_eq!(dialog.get_text(), "ab");
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_k_at_end_of_line_joins_without_arming() {
        // Cursor at end of line 1 of a two-line input: Ctrl+K deletes the
        // newline (joins lines) but does NOT touch yank, so don't arm.
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(shift_key(KeyCode::Enter));
        dialog.handle_key(key(KeyCode::Char('b')));
        dialog.handle_key(key(KeyCode::Up));
        dialog.handle_key(key(KeyCode::End));
        dialog.handle_key(ctrl_key(KeyCode::Char('k')));
        assert_eq!(dialog.get_text(), "ab");
        assert!(!dialog.restore_armed);
    }

    #[test]
    fn test_ctrl_u_multiline_kills_only_current_line_prefix() {
        // Ctrl+U on line 2 with text "bcd" and cursor mid-line deletes only
        // the line-2 prefix, and Ctrl+P restores exactly that.
        let mut dialog = SendMessageDialog::new("Test Session");
        dialog.handle_key(key(KeyCode::Char('a')));
        dialog.handle_key(shift_key(KeyCode::Enter));
        for c in "bcd".chars() {
            dialog.handle_key(key(KeyCode::Char(c)));
        }
        // Cursor at end of "bcd" (line 2, col 3). Move left once -> col 2.
        dialog.handle_key(key(KeyCode::Left));
        dialog.handle_key(ctrl_key(KeyCode::Char('u')));
        assert_eq!(dialog.get_text(), "a\nd");
        assert!(dialog.restore_armed);
        dialog.handle_key(ctrl_key(KeyCode::Char('p')));
        assert_eq!(dialog.get_text(), "a\nbcd");
    }
}
