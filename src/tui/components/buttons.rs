//! Centered "[Yes]    [No]" button row used by destructive-confirm dialogs.
//!
//! Used by `confirm`, `delete_options`, and `update_confirm`. If a fourth
//! caller needs a different button label set, generalize then.

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::components::hover::paint_hover_bg;
use crate::tui::styles::Theme;

/// Width of the rendered "[Yes]    [No]" row: 5 (Yes) + 4 spaces + 4
/// (No) = 13 cells. Kept as a constant so the click hit-test math
/// stays in lockstep with the renderer.
const YES_NO_ROW_WIDTH: u16 = 13;

/// Render a centered `[Yes]    [No]` row. Yes uses `theme.error`, No uses
/// `theme.running`; the unfocused button uses `theme.dimmed`. When
/// `hovered` is one of the returned button rects, it gets a
/// `theme.selection` background, the same highlight rows get elsewhere in
/// the TUI; callers pass the rect a `HoverState` resolved from the last
/// frame's `(yes_rect, no_rect)`. Returns `(yes_rect, no_rect)` covering
/// the visible glyphs, so callers that want mouse-clickable buttons can
/// hit-test the same cells the user sees. Both rects collapse to
/// zero-width if the row doesn't fit in `area` (a degenerate render the
/// caller can ignore).
pub fn render_yes_no(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    yes_focused: bool,
    hovered: Option<Rect>,
) -> (Rect, Rect) {
    let yes_style = if yes_focused {
        Style::default().fg(theme.error).bold()
    } else {
        Style::default().fg(theme.dimmed)
    };
    let no_style = if yes_focused {
        Style::default().fg(theme.dimmed)
    } else {
        Style::default().fg(theme.running).bold()
    };
    let line = Line::from(vec![
        Span::styled("[Yes]", yes_style),
        Span::raw("    "),
        Span::styled("[No]", no_style),
    ]);
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);

    if area.width < YES_NO_ROW_WIDTH || area.height == 0 {
        return (Rect::default(), Rect::default());
    }
    // Ratatui centers with `(width - line_len) / 2` for the left
    // offset; mirror that here so the rects line up with the actual
    // glyphs, not just the row.
    let left_pad = (area.width - YES_NO_ROW_WIDTH) / 2;
    let yes_x = area.x + left_pad;
    let no_x = yes_x + 9; // "[Yes]" + 4 spaces
    let yes_rect = Rect::new(yes_x, area.y, 5, 1);
    let no_rect = Rect::new(no_x, area.y, 4, 1);

    if let Some(rect) = hovered.filter(|r| *r == yes_rect || *r == no_rect) {
        paint_hover_bg(frame, rect, theme.selection);
    }

    (yes_rect, no_rect)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the row twice: first to learn the button rects, then with
    /// the `[No]` rect marked hovered. Assert the cells under "[No]" pick
    /// up the selection background while "[Yes]" keeps the default.
    #[test]
    fn hovered_button_gets_selection_background() {
        let theme = load_theme("empire");
        let backend = TestBackend::new(40, 3);
        let mut terminal = Terminal::new(backend).unwrap();

        let mut rects = (Rect::default(), Rect::default());
        terminal
            .draw(|f| {
                rects = render_yes_no(f, f.area(), &theme, false, None);
            })
            .unwrap();
        let (yes_rect, no_rect) = rects;

        terminal
            .draw(|f| {
                render_yes_no(f, f.area(), &theme, false, Some(no_rect));
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();

        assert_eq!(
            buf[(no_rect.x, no_rect.y)].bg,
            theme.selection,
            "hovered [No] should carry the selection background"
        );
        assert_ne!(
            buf[(yes_rect.x, yes_rect.y)].bg,
            theme.selection,
            "unhovered [Yes] should keep its default background"
        );
    }
}
