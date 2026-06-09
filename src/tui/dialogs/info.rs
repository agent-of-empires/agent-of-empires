//! Info dialog for displaying informational messages

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

/// Which end of an overflowing message stays visible. Most dialogs read
/// top-down, so `Top` is the default; error output (hook failures, panics)
/// puts the payload last and overrides to `Tail`.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum ScrollMode {
    #[default]
    Top,
    Tail,
}

pub struct InfoDialog {
    title: String,
    message: String,
    width: u16,
    height: u16,
    scroll_mode: ScrollMode,
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
            scroll_mode: ScrollMode::Top,
            dialog_area: Rect::default(),
            ok_button_area: Rect::default(),
            hover: HoverState::default(),
        }
    }

    /// Build a dialog sized to fit `message` after wrapping, for long
    /// multi-line content that would clip at the default 50x9.
    ///
    /// The message is pre-wrapped to the inner width here (so the row count
    /// is exact, not a byte-length estimate), and when it still exceeds the
    /// max dialog height the *head* is dropped behind a `… N earlier lines
    /// hidden` marker. Paragraph clips the bottom, and for error output the
    /// last lines are the payload (the panic message, the npm error
    /// summary), so overflow must eat the top, not the tail.
    ///
    /// 96, not 80: at the typical ~35-col sidebar width, a centered 80-wide
    /// dialog on a 150-col terminal lands its left border exactly at the
    /// sidebar's right border, which makes the modal visually blend into
    /// the layout. 96 shifts the coincidence point off the common
    /// laptop-fullscreen width and gives long path lines (e.g.
    /// `~/.config/agent-of-empires-dev`) more breathing room.
    pub fn sized_to_fit(title: &str, message: &str) -> Self {
        const WIDTH: u16 = 96;
        const MAX_HEIGHT: u16 = 35;
        // Rows available to the message at MAX_HEIGHT: borders, margin, and
        // the button row consume 6, and the height formula below keeps one
        // spare.
        const MAX_ROWS: usize = MAX_HEIGHT as usize - 7;
        let inner_width = WIDTH.saturating_sub(4) as usize;

        let mut rows = wrap_to_width(message, inner_width);
        if rows.len() > MAX_ROWS {
            let hidden = rows.len() - (MAX_ROWS - 1);
            rows.drain(..hidden);
            rows.insert(0, format!("… {} earlier lines hidden", hidden));
        }
        let height = ((rows.len() as u16).saturating_add(7)).clamp(9, MAX_HEIGHT);
        Self::new(title, &rows.join("\n"))
            .with_size(WIDTH, height)
            .with_scroll_mode(ScrollMode::Tail)
    }

    /// Choose which end of an overflowing message stays visible. Defaults to
    /// [`ScrollMode::Top`]; error dialogs override to [`ScrollMode::Tail`] so
    /// the trailing payload (panic message, hook output) isn't scrolled off.
    pub fn with_scroll_mode(mut self, mode: ScrollMode) -> Self {
        self.scroll_mode = mode;
        self
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

        // Message. Wrap to the *rendered* width (centered_rect may have
        // clamped below the requested size on small terminals) so the row
        // count is exact. In `Tail` mode scroll so the message's tail stays
        // visible when it doesn't fit (error output puts the payload last:
        // the panic message, the npm error summary); `Top` mode keeps the
        // head anchored and clips the bottom, the default for read-top-down
        // dialogs.
        let rows = wrap_to_width(&self.message, chunks[0].width as usize);
        let scroll = match self.scroll_mode {
            ScrollMode::Top => 0,
            ScrollMode::Tail => (rows.len() as u16).saturating_sub(chunks[0].height),
        };
        let message = Paragraph::new(rows.join("\n"))
            .style(Style::default().fg(theme.text))
            .scroll((scroll, 0));
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

/// Wrap `message` to `width` display columns: break at the last space on
/// the row when there is one, mid-word otherwise (long paths in stack
/// traces exceed any width). Every output row fits in `width`, so the row
/// count is exact for sizing and scroll math; leading whitespace
/// (stack-trace indentation) is preserved.
fn wrap_to_width(message: &str, width: usize) -> Vec<String> {
    use unicode_width::UnicodeWidthChar;
    let width = width.max(1);
    let mut rows = Vec::new();
    for line in message.lines() {
        let mut row = String::new();
        let mut row_w = 0usize;
        for ch in line.chars() {
            let w = ch.width().unwrap_or(0);
            if row_w + w > width && !row.is_empty() {
                // Prefer a word boundary; carry the partial word over.
                if let Some(pos) = row.rfind(' ') {
                    let carry = row.split_off(pos + 1);
                    rows.push(std::mem::take(&mut row));
                    row_w = carry.chars().filter_map(|c| c.width()).sum();
                    row = carry;
                } else {
                    rows.push(std::mem::take(&mut row));
                    row_w = 0;
                }
            }
            row.push(ch);
            row_w += w;
        }
        rows.push(row);
    }
    if rows.is_empty() {
        rows.push(String::new());
    }
    rows
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
    fn wrap_breaks_long_lines_at_display_width() {
        let rows = wrap_to_width(&"x".repeat(25), 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].len(), 10);
        assert_eq!(rows[2].len(), 5);
    }

    #[test]
    fn wrap_breaks_at_word_boundary_when_possible() {
        let rows = wrap_to_width("aaa bbb ccc", 7);
        assert_eq!(rows, vec!["aaa ", "bbb ccc"]);
    }

    #[test]
    fn wrap_preserves_short_and_empty_lines() {
        let rows = wrap_to_width("short\n\n  indented", 92);
        assert_eq!(rows, vec!["short", "", "  indented"]);
    }

    #[test]
    fn wrap_counts_wide_chars_by_columns() {
        // 6 CJK chars = 12 columns; at width 10 only 5 fit per row.
        let rows = wrap_to_width(&"漢".repeat(6), 10);
        assert_eq!(rows, vec!["漢".repeat(5), "漢".to_string()]);
    }

    #[test]
    fn sized_to_fit_short_message_keeps_min_height() {
        let dialog = InfoDialog::sized_to_fit("T", "one line");
        assert_eq!(dialog.height, 9);
        assert_eq!(dialog.message, "one line");
    }

    #[test]
    fn sized_to_fit_overflow_keeps_tail_and_marks_hidden_head() {
        // 60 numbered lines: far more than the 28-row budget at max height.
        let message: String = (1..=60)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let dialog = InfoDialog::sized_to_fit("T", &message);
        assert_eq!(dialog.height, 35);
        // The tail (the actual error in hook output) must survive; the head
        // is replaced by the hidden-lines marker.
        assert!(dialog.message.ends_with("line 60"), "{}", dialog.message);
        assert!(
            dialog.message.starts_with("… 33 earlier lines hidden"),
            "{}",
            dialog.message
        );
        assert!(!dialog.message.contains("line 33\n"), "{}", dialog.message);
        assert!(dialog.message.contains("line 34\n"), "{}", dialog.message);
        // Exactly MAX_ROWS rows: marker + 27 tail lines.
        assert_eq!(dialog.message.lines().count(), 28);
    }

    #[test]
    fn sized_to_fit_counts_wrapped_rows_not_logical_lines() {
        // One logical line that wraps to 3 visual rows at inner width 92.
        let dialog = InfoDialog::sized_to_fit("T", &"x".repeat(92 * 2 + 1));
        assert_eq!(dialog.message.lines().count(), 3);
        assert_eq!(dialog.height, 10); // 3 rows + 7
    }

    /// On a terminal shorter than the dialog, the message must scroll so
    /// its tail (the actual error in hook output) stays visible instead of
    /// clipping at the bottom.
    #[test]
    fn short_terminal_shows_message_tail() {
        use ratatui::backend::TestBackend;

        let message: String = (1..=28)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut dialog = InfoDialog::sized_to_fit("T", &message);
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area, &Theme::default());
            })
            .unwrap();

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("line 28"), "tail must be visible");
        assert!(!rendered.contains("line 1 "), "head should scroll away");
    }

    /// Default (`Top`) dialogs keep the head anchored when content overflows,
    /// so the first lines of a top-down message (e.g. a warning list) stay
    /// visible and the bottom clips instead. Guards against the error-dialog
    /// tail-scroll leaking onto every `InfoDialog::new` caller.
    #[test]
    fn default_dialog_anchors_head_on_overflow() {
        use ratatui::backend::TestBackend;

        let message: String = (1..=30)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        let mut dialog = InfoDialog::new("T", &message);
        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area, &Theme::default());
            })
            .unwrap();

        let rendered = format!("{:?}", terminal.backend().buffer());
        assert!(rendered.contains("line 1 "), "head must stay visible");
        assert!(!rendered.contains("line 30"), "tail should clip away");
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
