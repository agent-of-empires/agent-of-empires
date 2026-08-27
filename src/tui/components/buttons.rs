//! Centered confirm/cancel button row used by destructive-confirm dialogs.
//!
//! Used by `confirm`, `delete_options`, and `update_confirm`. The default
//! labels are `[Yes]` / `[No]`; a dialog whose question can't be answered
//! by "Yes" alone passes its own verbs to [`render_buttons`].

use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::components::hover::paint_hover_bg;
use crate::tui::styles::Theme;

/// Blank cells between the two buttons. Part of the hit-test math, so it
/// stays in lockstep with the rendered row.
const BUTTON_GAP: u16 = 4;

/// Render a centered `[Yes]    [No]` row. See [`render_buttons`]; this is
/// the default-label spelling of it.
pub fn render_yes_no(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    yes_focused: bool,
    hovered: Option<Rect>,
) -> (Rect, Rect) {
    render_buttons(frame, area, theme, ("Yes", "No"), yes_focused, hovered)
}

/// Render a centered `[<yes>]    [<no>]` row. The confirm button uses
/// `theme.error`, the cancel button `theme.running`; the unfocused one uses
/// `theme.dimmed`. When `hovered` is one of the returned button rects, it
/// gets a `theme.selection` background, the same highlight rows get
/// elsewhere in the TUI; callers pass the rect a `HoverState` resolved from
/// the last frame's `(yes_rect, no_rect)`. Returns `(yes_rect, no_rect)`
/// covering the visible glyphs, so callers that want mouse-clickable
/// buttons can hit-test the same cells the user sees. Both rects collapse
/// to zero-width if the row doesn't fit in `area` (a degenerate render the
/// caller can ignore).
pub fn render_buttons(
    frame: &mut Frame,
    area: Rect,
    theme: &Theme,
    labels: (&str, &str),
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
    let yes_text = format!("[{}]", labels.0);
    let no_text = format!("[{}]", labels.1);
    let line = Line::from(vec![
        Span::styled(yes_text.clone(), yes_style),
        Span::raw(" ".repeat(BUTTON_GAP as usize)),
        Span::styled(no_text.clone(), no_style),
    ]);
    frame.render_widget(Paragraph::new(line).alignment(Alignment::Center), area);

    // Labels are ASCII verbs, so a char count is the cell count.
    let yes_width = yes_text.chars().count() as u16;
    let no_width = no_text.chars().count() as u16;
    let row_width = yes_width + BUTTON_GAP + no_width;
    if area.width < row_width || area.height == 0 {
        return (Rect::default(), Rect::default());
    }
    // Ratatui's `get_line_offset` centers with `width / 2 - line_width / 2`,
    // which is NOT `(width - line_width) / 2`: the two disagree by a cell
    // whenever the row is odd and the area even (a 13-cell "[Yes]    [No]"
    // in a 48-wide dialog, say). Mirror ratatui exactly so the rects land on
    // the glyphs the user sees and a click on the last bracket registers.
    let left_pad = (area.width / 2).saturating_sub(row_width / 2);
    let yes_x = area.x + left_pad;
    let no_x = yes_x + yes_width + BUTTON_GAP;
    let yes_rect = Rect::new(yes_x, area.y, yes_width, 1);
    let no_rect = Rect::new(no_x, area.y, no_width, 1);

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

    /// The returned hit-rects must cover the glyphs actually drawn, for
    /// default and custom labels alike: a rect that drifts from its label
    /// makes a click on `[Cancel]` fire the confirm.
    #[test]
    fn hit_rects_cover_the_rendered_labels() {
        let theme = load_theme("empire");
        // Odd and even row widths, so the centering division is exercised
        // both with and without a remainder.
        for width in [40, 41] {
            for labels in [("Yes", "No"), ("Delete", "Cancel")] {
                let backend = TestBackend::new(width, 3);
                let mut terminal = Terminal::new(backend).unwrap();
                let mut rects = (Rect::default(), Rect::default());
                terminal
                    .draw(|f| {
                        rects = render_buttons(f, f.area(), &theme, labels, false, None);
                    })
                    .unwrap();
                let buf = terminal.backend().buffer().clone();

                let read = |rect: Rect| -> String {
                    (rect.x..rect.x + rect.width)
                        .map(|x| buf[(x, rect.y)].symbol())
                        .collect()
                };
                assert_eq!(read(rects.0), format!("[{}]", labels.0), "width {width}");
                assert_eq!(read(rects.1), format!("[{}]", labels.1), "width {width}");
            }
        }
    }

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
