//! Shared mouse-hover highlight helpers for dialogs.
//!
//! Dialogs capture a `Rect` per clickable element during `render` (to
//! hit-test clicks); `HoverState` reuses those same rects to track which
//! one the pointer is over, and `paint_hover_bg` highlights it. The
//! highlight is visual only: hover never moves keyboard focus, because
//! mouse drift between reading a prompt and pressing Enter would
//! otherwise flip which action fires.

use ratatui::layout::Position;
use ratatui::prelude::*;

/// Paint `bg` behind every cell of `area`, leaving glyphs and their
/// foreground colors intact. The same treatment rows get under the
/// cursor in the settings and home views.
pub fn paint_hover_bg(frame: &mut Frame, area: Rect, bg: Color) {
    let buf = frame.buffer_mut();
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.set_bg(bg);
            }
        }
    }
}

/// Tracks which of a dialog's clickable rects the pointer is over so the
/// renderer can highlight it. Feed it the same rects the click
/// hit-test uses; it stays empty until the dialog has rendered once
/// (every rect is zero-sized before then, and a zero-area rect contains
/// no point).
#[derive(Default)]
pub struct HoverState {
    hovered: Option<Rect>,
}

impl HoverState {
    /// The rect under the cursor, if any. Pass to a renderer (or
    /// `paint_hover_bg`) to highlight it.
    pub fn current(&self) -> Option<Rect> {
        self.hovered
    }

    /// Like [`current`](Self::current), but only when the hovered rect is
    /// still one of `rects`. Paint sites pass the rects they computed this
    /// frame so a stale rect from a previous layout (e.g. a terminal
    /// resize with no intervening mouse move) is dropped instead of
    /// tinting the wrong cells.
    pub fn current_in(&self, rects: &[Rect]) -> Option<Rect> {
        self.hovered.filter(|r| rects.contains(r))
    }

    /// Recompute the hovered rect from a pointer position against the
    /// clickable rects captured on the previous frame. Returns `true`
    /// when the hovered rect changed, so callers redraw only on a real
    /// move between targets, not on every pixel twitch.
    pub fn update(&mut self, col: u16, row: u16, rects: &[Rect]) -> bool {
        let pos = Position::from((col, row));
        let new = rects.iter().copied().find(|r| r.contains(pos));
        if self.hovered == new {
            return false;
        }
        self.hovered = new;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracks_rect_under_cursor_and_reports_changes() {
        let mut hover = HoverState::default();
        let a = Rect::new(10, 8, 5, 1);
        let b = Rect::new(19, 8, 4, 1);
        assert_eq!(hover.current(), None);

        // Moving onto `a` is a change.
        assert!(hover.update(12, 8, &[a, b]));
        assert_eq!(hover.current(), Some(a));

        // Staying within `a` is not.
        assert!(!hover.update(11, 8, &[a, b]));

        // Crossing to `b` is a change.
        assert!(hover.update(20, 8, &[a, b]));
        assert_eq!(hover.current(), Some(b));

        // The gap between the rects clears the highlight.
        assert!(hover.update(16, 8, &[a, b]));
        assert_eq!(hover.current(), None);
    }

    #[test]
    fn zero_sized_rects_never_match() {
        // Before the first render every captured rect is the default
        // zero-sized rect; nothing should register as hovered, not even
        // the origin.
        let mut hover = HoverState::default();
        assert!(!hover.update(0, 0, &[Rect::default(), Rect::default()]));
        assert_eq!(hover.current(), None);
    }
}
