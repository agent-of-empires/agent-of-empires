//! Footer key hints that double as buttons. A keyboard-driven dialog's
//! `handle_click` returns the clicked hint's key, and the caller presses it
//! through the dialog's `handle_key`, so clicks share the keyboard's handling.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::dialogs::{centered_x, hit, row_index, target_rects};
use crate::tui::styles::Theme;

/// `(key label, action label, key pressed)`. `KeyCode::Null` is display-only:
/// for focus-dependent keys (Space types into a text field), and for Enter in
/// lists whose hover moves the selection, where reaching the hint would cross
/// rows.
pub type Hint<'a> = (&'a str, &'a str, KeyCode);

const GAP: u16 = 2;

/// A footer row of [`Hint`]s such as `Enter select  Esc close`, keys in
/// `theme.hint`, that records where each one landed.
#[derive(Default)]
pub struct HintButtons {
    targets: Vec<(KeyCode, Rect)>,
    hover: HoverState,
}

impl HintButtons {
    /// Draw `hints` on the first row of `area` and record a hit rect per hint.
    /// Hints that do not fit are left out rather than clipped mid-label.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        hints: &[Hint],
        alignment: Alignment,
    ) {
        self.targets.clear();
        if area.height == 0 {
            return;
        }
        let mut spans = Vec::with_capacity(hints.len() * 3);
        let mut placed = Vec::with_capacity(hints.len());
        let mut used: u16 = 0;
        for (key, label, code) in hints {
            let width = (key.chars().count() + 1 + label.chars().count()) as u16;
            let sep = if spans.is_empty() { 0 } else { GAP };
            if used + sep + width > area.width {
                break;
            }
            if sep > 0 {
                spans.push(Span::raw(" ".repeat(sep as usize)));
            }
            spans.push(Span::styled(
                key.to_string(),
                Style::default().fg(theme.hint),
            ));
            spans.push(Span::raw(format!(" {label}")));
            placed.push((*code, used + sep, width));
            used += sep + width;
        }
        let x = match alignment {
            Alignment::Center => centered_x(area, used),
            Alignment::Right => area.right().saturating_sub(used),
            Alignment::Left => area.x,
        };
        let row = Rect { height: 1, ..area };
        frame.render_widget(Paragraph::new(Line::from(spans)).alignment(alignment), row);
        self.targets = placed
            .into_iter()
            .filter(|(code, _, _)| *code != KeyCode::Null)
            .map(|(code, offset, width)| (code, Rect::new(x + offset, area.y, width, 1)))
            .collect();
        if let Some(rect) = self.hover.current_in(&target_rects(&self.targets)) {
            paint_hover_bg(frame, rect, theme.selection);
        }
    }

    /// Forget the drawn hints, for a frame that draws none.
    pub fn clear(&mut self) {
        self.targets.clear();
    }

    /// The key a click at `(col, row)` presses, if it hit a hint.
    pub fn key_at(&self, col: u16, row: u16) -> Option<KeyEvent> {
        hit(&self.targets, col, row).map(KeyEvent::from)
    }

    /// Track the hovered hint; true when it changed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.hover.update(col, row, &target_rects(&self.targets))
    }
}

/// Mouse state for a list over a `·`-separated footer (plugin and skills
/// managers). Rows only tint on hover: footer actions act on the selection,
/// which the pointer must not retarget on its way to a hint.
#[derive(Default)]
pub struct ListMouse {
    list: Rect,
    offset: usize,
    hints: Vec<(KeyEvent, Rect)>,
    hover: HoverState,
}

impl ListMouse {
    /// Forget the last frame's targets; call at the start of a render.
    pub fn reset(&mut self) {
        self.list = Rect::default();
        self.hints.clear();
    }

    /// The drawn list area and its scroll offset (from `ListState::offset`).
    pub fn record_list(&mut self, area: Rect, offset: usize) {
        self.list = area;
        self.offset = offset;
    }

    /// Replace the hints with those drawn in `area`; a popup calls this after
    /// the panel so only its own hints stay clickable.
    pub fn record_hints(&mut self, buf: &Buffer, area: Rect) {
        self.hints = scan_dot_hints(buf, area);
    }

    pub fn clear_hints(&mut self) {
        self.hints.clear();
    }

    pub fn hint_at(&self, col: u16, row: u16) -> Option<KeyEvent> {
        hit(&self.hints, col, row)
    }

    /// A click on a list of `len` rows: the first selects the row under the
    /// pointer, a second on the selected row returns Enter to open it.
    pub fn click_row(
        &self,
        col: u16,
        row: u16,
        len: usize,
        selected: &mut usize,
    ) -> Option<KeyEvent> {
        let idx = self.offset + row_index(self.list, col, row, len.saturating_sub(self.offset))?;
        if *selected == idx {
            return Some(KeyEvent::from(KeyCode::Enter));
        }
        *selected = idx;
        None
    }

    fn row_rects(&self, len: usize) -> impl Iterator<Item = Rect> + '_ {
        let list = self.list;
        (0..len.saturating_sub(self.offset))
            .take(list.height as usize)
            .map(move |i| Rect::new(list.x, list.y + i as u16, list.width, 1))
    }

    /// Hover targets: the hints, plus the rows while `rows_live` (no popup).
    fn hover_rects(&self, len: usize, rows_live: bool) -> Vec<Rect> {
        let mut rects = target_rects(&self.hints);
        if rows_live {
            rects.extend(self.row_rects(len));
        }
        rects
    }

    /// True when the hovered target changed.
    pub fn handle_hover(&mut self, col: u16, row: u16, len: usize, rows_live: bool) -> bool {
        let rects = self.hover_rects(len, rows_live);
        self.hover.update(col, row, &rects)
    }

    /// Tint the hovered target; call at the end of a render.
    pub fn paint_hover(&self, frame: &mut Frame, theme: &Theme, len: usize, rows_live: bool) {
        if let Some(rect) = self.hover.current_in(&self.hover_rects(len, rows_live)) {
            paint_hover_bg(frame, rect, theme.selection);
        }
    }
}

/// Targets for a drawn `key action · key action` footer, following its
/// wrapping. A segment led by `enter`, `esc`, `space`, `tab`, `ctrl+x` or a
/// single character presses that key; others (`j/k`) stay inert.
pub fn scan_dot_hints(buf: &Buffer, area: Rect) -> Vec<(KeyEvent, Rect)> {
    let area = area.intersection(buf.area);
    let mut hints = Vec::new();
    for y in area.y..area.bottom() {
        let cells: Vec<(u16, &str)> = (area.x..area.right())
            .map(|x| (x, buf[(x, y)].symbol()))
            .collect();
        for segment in cells.split(|(_, sym)| *sym == "·") {
            let Some(start) = segment.iter().position(|(_, sym)| !sym.trim().is_empty()) else {
                continue;
            };
            let end = segment
                .iter()
                .rposition(|(_, sym)| !sym.trim().is_empty())
                .unwrap_or(start);
            let text: String = segment[start..=end].iter().map(|(_, sym)| *sym).collect();
            let Some(key) = text.split_whitespace().next().and_then(parse_key_token) else {
                continue;
            };
            let x = segment[start].0;
            hints.push((key, Rect::new(x, y, segment[end].0 - x + 1, 1)));
        }
    }
    hints
}

/// Like [`scan_dot_hints`] for hints separated by two or more spaces, as in
/// `[L] Local`, `[Esc close]` or `R: restart`; brackets and a trailing colon
/// are stripped from the key token.
pub fn scan_spaced_hints(buf: &Buffer, area: Rect) -> Vec<(KeyEvent, Rect)> {
    let area = area.intersection(buf.area);
    let mut hints = Vec::new();
    for y in area.y..area.bottom() {
        let blank = |x: u16| buf[(x, y)].symbol().trim().is_empty();
        let mut x = area.x;
        while x < area.right() {
            if blank(x) {
                x += 1;
                continue;
            }
            // A segment runs until two blank cells in a row.
            let start = x;
            let mut end = x;
            while x < area.right() && !(blank(x) && (x + 1 >= area.right() || blank(x + 1))) {
                if !blank(x) {
                    end = x;
                }
                x += 1;
            }
            let text: String = (start..=end).map(|cx| buf[(cx, y)].symbol()).collect();
            let token = text
                .split_whitespace()
                .next()
                .unwrap_or("")
                .trim_start_matches('[')
                .trim_end_matches([']', ':']);
            if let Some(key) = parse_key_token(token) {
                hints.push((key, Rect::new(start, y, end - start + 1, 1)));
            }
        }
    }
    hints
}

fn parse_key_token(token: &str) -> Option<KeyEvent> {
    let lower = token.to_ascii_lowercase();
    let code = match lower.as_str() {
        "enter" => KeyCode::Enter,
        "esc" => KeyCode::Esc,
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        _ => {
            if let Some(c) = lower.strip_prefix("ctrl+").and_then(single_char) {
                return Some(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));
            }
            KeyCode::Char(single_char(token)?)
        }
    };
    Some(KeyEvent::from(code))
}

fn single_char(s: &str) -> Option<char> {
    let mut chars = s.chars();
    let c = chars.next()?;
    chars.next().is_none().then_some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::dialogs::test_render::{draw, find};

    #[test]
    fn hint_rects_cover_the_drawn_labels_for_every_alignment() {
        let hints: &[Hint] = &[
            ("Space", "toggle", KeyCode::Null),
            ("Enter", "select", KeyCode::Enter),
            ("Esc", "close", KeyCode::Esc),
        ];
        // (width, alignment); 26 fits only the first two hints.
        for (width, alignment) in [
            (50, Alignment::Left),
            (50, Alignment::Center),
            (51, Alignment::Center),
            (50, Alignment::Right),
            (26, Alignment::Left),
        ] {
            let mut buttons = HintButtons::default();
            let buf = draw(width, 1, |f, theme| {
                buttons.render(f, f.area(), theme, hints, alignment)
            });
            let (x, _) = find(&buf, "Space toggle");
            assert_eq!(buttons.key_at(x, 0), None, "a Null hint is display-only");
            let (x, _) = find(&buf, "Enter select");
            for col in [x, x + 11] {
                assert_eq!(buttons.key_at(col, 0).map(|k| k.code), Some(KeyCode::Enter));
            }
            assert_eq!(buttons.key_at(x + 12, 0), None, "{alignment:?} {width}");
            let fits = width > 26;
            assert_eq!(buttons.targets.len(), 1 + usize::from(fits), "{width}");
            assert!(buttons.handle_hover(x, 0));
        }
    }

    #[test]
    fn scanned_hints_map_each_drawn_segment_to_its_key() {
        use ratatui::widgets::Wrap;
        let ctrl_s = KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL);
        let key = |c: KeyCode| KeyEvent::from(c);
        type Scan = fn(&Buffer, Rect) -> Vec<(KeyEvent, Rect)>;
        type Case<'a> = (&'a [&'a str], u16, Scan, Vec<(u16, u16, u16, KeyEvent)>);
        // (lines, width, scanner, expected (x, y, width, key)); inert tokens
        // such as `j/k` and `[←/→]` produce nothing, and wrapping is followed.
        let cases: [Case; 2] = [
            (
                &["enter view · ctrl+s save · j/k scroll · A always · esc close"],
                40,
                scan_dot_hints,
                vec![
                    (0, 0, 10, key(KeyCode::Enter)),
                    (13, 0, 11, ctrl_s),
                    (0, 1, 8, key(KeyCode::Char('A'))),
                    (11, 1, 9, key(KeyCode::Esc)),
                ],
            ),
            (
                &[
                    "[←/→] choose    [L] Local    [Enter] confirm",
                    "Elapsed: 5s    [Esc close]  [S stop]",
                    "Tab: URL  ?: help",
                ],
                50,
                scan_spaced_hints,
                vec![
                    (16, 0, 9, key(KeyCode::Char('L'))),
                    (29, 0, 15, key(KeyCode::Enter)),
                    (15, 1, 11, key(KeyCode::Esc)),
                    (28, 1, 8, key(KeyCode::Char('S'))),
                    (0, 2, 8, key(KeyCode::Tab)),
                    (10, 2, 7, key(KeyCode::Char('?'))),
                ],
            ),
        ];
        for (lines, width, scan, want) in cases {
            let text: Vec<Line> = lines.iter().map(|l| Line::from(*l)).collect();
            let buf = draw(width, 3, |f, _| {
                f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), f.area())
            });
            let got: Vec<_> = scan(&buf, buf.area)
                .into_iter()
                .map(|(k, r)| (r.x, r.y, r.width, k))
                .collect();
            assert_eq!(got, want, "{lines:?}");
        }
    }
}
