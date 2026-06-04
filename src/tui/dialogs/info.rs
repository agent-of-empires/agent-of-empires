//! Info dialog for displaying informational messages

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

pub struct InfoDialog {
    title: String,
    message: String,
    width: u16,
    height: u16,
    dialog_area: Rect,
    /// Rect of the `[OK]` button, captured during `render`. A click
    /// anywhere dismisses, but the button is the call to action, so it
    /// picks up the hover highlight to read as clickable.
    ok_button_area: Rect,
    /// Whether the cursor is over `[OK]`, for the hover highlight.
    hover: HoverState,
}

impl InfoDialog {
    pub fn new(title: &str, message: &str) -> Self {
        Self {
            title: title.to_string(),
            message: message.to_string(),
            width: 50,
            height: 9,
            dialog_area: Rect::default(),
            ok_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    /// A left-click anywhere inside the info dialog dismisses it,
    /// matching the keyboard's "any of Esc/Enter/Space closes" model.
    /// `None` when the click landed outside the dialog area, so the
    /// caller can decide whether to swallow it anyway.
    pub fn handle_click(&self, col: u16, row: u16) -> Option<DialogResult<()>> {
        if self
            .dialog_area
            .contains(ratatui::layout::Position::from((col, row)))
        {
            Some(DialogResult::Cancel)
        } else {
            None
        }
    }

    /// Highlight the `[OK]` button when the cursor is over it. A click
    /// anywhere still dismisses via `handle_click`; this only signals the
    /// call to action. Returns `true` when the highlight changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover.update(col, row, &[self.ok_button_area])
    }

    /// Customize the dialog's footprint. Useful for long, multi-paragraph
    /// messages (e.g. the startup config-warning) that would clip at the
    /// default 50x9.
    pub fn with_size(mut self, width: u16, height: u16) -> Self {
        self.width = width;
        self.height = height;
        self
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> DialogResult<()> {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char(' ') => DialogResult::Cancel,
            _ => DialogResult::Continue,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme) {
        let dialog_area = super::centered_rect(area, self.width, self.height);
        self.dialog_area = dialog_area;

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(theme.border))
            .title(format!(" {} ", self.title))
            .title_style(Style::default().fg(theme.title).bold());

        let inner = block.inner(dialog_area);
        frame.render_widget(block, dialog_area);

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([Constraint::Min(1), Constraint::Length(2)])
            .split(inner);

        // Message
        let message = Paragraph::new(&*self.message)
            .style(Style::default().fg(theme.text))
            .wrap(Wrap { trim: true });
        frame.render_widget(message, chunks[0]);

        // OK button. Click is handled by the whole-dialog hit region in
        // `handle_click`; the rect is captured only so hover can
        // highlight the button as the call to action.
        let button = Line::from(vec![Span::styled(
            "[OK]",
            Style::default().fg(theme.accent).bold(),
        )]);
        let button_area = chunks[1];
        const OK_WIDTH: u16 = 4; // "[OK]"
        self.ok_button_area = if button_area.width >= OK_WIDTH {
            let ok_x = button_area.x + (button_area.width - OK_WIDTH) / 2;
            Rect::new(ok_x, button_area.y, OK_WIDTH, 1)
        } else {
            Rect::default()
        };
        frame.render_widget(
            Paragraph::new(button).alignment(Alignment::Center),
            button_area,
        );

        if let Some(rect) = self.hover.current_in(&[self.ok_button_area]) {
            paint_hover_bg(frame, rect, theme.selection);
        }
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
    fn test_esc_closes() {
        let mut dialog = InfoDialog::new("Test", "Message");
        let result = dialog.handle_key(key(KeyCode::Esc));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_enter_closes() {
        let mut dialog = InfoDialog::new("Test", "Message");
        let result = dialog.handle_key(key(KeyCode::Enter));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_space_closes() {
        let mut dialog = InfoDialog::new("Test", "Message");
        let result = dialog.handle_key(key(KeyCode::Char(' ')));
        assert!(matches!(result, DialogResult::Cancel));
    }

    #[test]
    fn test_other_keys_continue() {
        let mut dialog = InfoDialog::new("Test", "Message");
        let result = dialog.handle_key(key(KeyCode::Char('x')));
        assert!(matches!(result, DialogResult::Continue));
    }

    #[test]
    fn hover_highlights_ok_button() {
        // Stage the button rect manually; the real one comes from render().
        let mut dialog = InfoDialog::new("Test", "Message");
        dialog.ok_button_area = Rect::new(10, 8, 4, 1);

        // Over [OK]: highlight it.
        assert!(dialog.handle_hover(11, 8));
        assert_eq!(dialog.hover.current(), Some(dialog.ok_button_area));

        // Off the button clears the highlight.
        assert!(dialog.handle_hover(0, 0));
        assert_eq!(dialog.hover.current(), None);
    }
}
