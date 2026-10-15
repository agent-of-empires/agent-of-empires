//! Info dialog.

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::prelude::*;
use ratatui::widgets::*;

use super::DialogResult;
use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::styles::Theme;

/// Which end of an overflowing message stays visible. `Top` by default; error
/// output puts its payload last and overrides to `Tail`.
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
    /// The `[OK]` button. A click anywhere dismisses, but the button takes
    /// the hover highlight so it reads as clickable.
    ok_button_area: Rect,
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

    /// A dialog sized to fit `message` after wrapping, for content that would
    /// clip at the default 50x9. Wrapping happens here so the row count is
    /// exact, and an over-tall message loses its *head* behind a `… N earlier
    /// lines hidden` marker: error payloads sit in the last lines.
    ///
    /// 96 wide, not 80: at a typical sidebar width a centered 80-wide dialog
    /// on a 150-column terminal lands its border exactly on the sidebar's, and
    /// the modal blends into the layout.
    pub fn sized_to_fit(title: &str, message: &str) -> Self {
        const WIDTH: u16 = 96;
        const MAX_HEIGHT: u16 = 35;
        // Borders, margin and the button row consume 6 rows, and the height
        // formula below keeps one spare.
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

    /// Which end of an overflowing message stays visible.
    pub fn with_scroll_mode(mut self, mode: ScrollMode) -> Self {
        self.scroll_mode = mode;
        self
    }

    /// Read by the tick loop to pick an auto-dismiss path on a recovery edge.
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Compared by the tick loop against the current `reload_failure_state`,
    /// so an open `Reload Failed` dialog refreshes only when the failing
    /// source set changes.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// A click anywhere inside dismisses, as any of Esc/Enter/Space does.
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
        let block =
            super::toned_dialog_block(format!(" {} ", self.title), theme.border, theme.title);
        let (dialog_area, inner) =
            super::render_dialog_frame(frame, area, self.width, self.height, block);
        self.dialog_area = dialog_area;

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
    use crate::tui::dialogs::test_keys::key;

    /// Render `dialog` into a short terminal and return the buffer's debug
    /// form, which is enough to look for line markers.
    fn rendered(dialog: &mut InfoDialog) -> String {
        use ratatui::backend::TestBackend;

        let mut terminal = Terminal::new(TestBackend::new(120, 20)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                dialog.render(frame, area, &Theme::default());
            })
            .unwrap();
        format!("{:?}", terminal.backend().buffer())
    }

    fn numbered(lines: usize) -> String {
        (1..=lines)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn any_dismiss_key_closes_and_others_do_not() {
        for code in [KeyCode::Esc, KeyCode::Enter, KeyCode::Char(' ')] {
            assert!(
                matches!(
                    InfoDialog::new("Test", "Message").handle_key(key(code)),
                    DialogResult::Cancel
                ),
                "{code:?}"
            );
        }
        assert!(matches!(
            InfoDialog::new("Test", "Message").handle_key(key(KeyCode::Char('x'))),
            DialogResult::Continue
        ));

        // Staged manually; the real rect comes from render().
        let mut dialog = InfoDialog::new("Test", "Message");
        dialog.ok_button_area = Rect::new(10, 8, 4, 1);
        assert!(dialog.handle_hover(11, 8));
        assert_eq!(dialog.hover.current(), Some(dialog.ok_button_area));
        assert!(dialog.handle_hover(0, 0));
        assert_eq!(dialog.hover.current(), None);
    }

    #[test]
    fn wrapping_counts_display_columns_and_prefers_word_boundaries() {
        assert_eq!(wrap_to_width("aaa bbb ccc", 7), vec!["aaa ", "bbb ccc"]);
        assert_eq!(
            wrap_to_width("short\n\n  indented", 92),
            vec!["short", "", "  indented"]
        );

        let rows = wrap_to_width(&"x".repeat(25), 10);
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].len(), 10);
        assert_eq!(rows[2].len(), 5);

        // Six CJK chars are twelve columns, so only five fit in a row of ten.
        assert_eq!(
            wrap_to_width(&"漢".repeat(6), 10),
            vec!["漢".repeat(5), "漢".to_string()]
        );
    }

    #[test]
    fn sized_to_fit_measures_wrapped_rows_and_drops_the_head_when_over_budget() {
        let dialog = InfoDialog::sized_to_fit("T", "one line");
        assert_eq!(dialog.height, 9, "a short message keeps the minimum height");
        assert_eq!(dialog.message, "one line");

        // One logical line that wraps to three rows at inner width 92.
        let dialog = InfoDialog::sized_to_fit("T", &"x".repeat(92 * 2 + 1));
        assert_eq!(dialog.message.lines().count(), 3);
        assert_eq!(dialog.height, 10);

        // Far past the 28-row budget: the tail carries the actual error, so
        // the head goes behind the hidden-lines marker.
        let dialog = InfoDialog::sized_to_fit("T", &numbered(60));
        assert_eq!(dialog.height, 35);
        assert_eq!(dialog.message.lines().count(), 28, "marker + 27 tail lines");
        assert!(dialog.message.ends_with("line 60"), "{}", dialog.message);
        assert!(
            dialog.message.starts_with("… 33 earlier lines hidden"),
            "{}",
            dialog.message
        );
        assert!(!dialog.message.contains("line 33\n"), "{}", dialog.message);
        assert!(dialog.message.contains("line 34\n"), "{}", dialog.message);
    }

    #[test]
    fn overflow_anchors_the_head_by_default_and_the_tail_for_errors() {
        // A terminal shorter than the dialog must not clip away the end of a
        // `sized_to_fit` error, where the payload lives.
        let mut error = InfoDialog::sized_to_fit("T", &numbered(28));
        let screen = rendered(&mut error);
        assert!(screen.contains("line 28"), "the tail must be visible");
        assert!(!screen.contains("line 1 "), "the head should scroll away");

        // A plain dialog reads top-down, so it keeps its head instead.
        let mut plain = InfoDialog::new("T", &numbered(30));
        let screen = rendered(&mut plain);
        assert!(screen.contains("line 1 "), "the head must stay visible");
        assert!(!screen.contains("line 30"), "the tail should clip away");
    }
}
