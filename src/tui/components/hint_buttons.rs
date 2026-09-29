//! Footer key hints that double as buttons: clicking `Esc close` presses Esc.
//!
//! Keyboard-driven dialogs render their actions as hints; routing a click on
//! a hint through the dialog's own `handle_key` gives mouse users the same
//! actions without a second code path.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::prelude::*;
use ratatui::widgets::Paragraph;

use crate::tui::components::hover::{paint_hover_bg, HoverState};
use crate::tui::dialogs::{centered_x, hit, row_index, target_rects};
use crate::tui::styles::Theme;

/// One hint: the key label shown, the action label, and the key it presses.
/// `KeyCode::Null` draws the hint without making it clickable: for keys whose
/// meaning depends on focus (Space types into a text field), and for Enter in
/// lists whose hover moves the selection, where the pointer would cross rows
/// on its way to the hint (a row click picks it directly).
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

/// Mouse state for a panel of one-row list items over a `·`-separated
/// footer, as the plugin and skills managers draw. Rows hover-tint only:
/// footer actions act on the selected row, so the pointer crossing rows on its
/// way to a hint must not retarget them.
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

/// Hit rects for an already drawn `key action · key action` footer in `area`.
/// Each segment presses its leading key token (`enter`, `esc`, `space`,
/// `tab`, `ctrl+s`, or a single character); segments led by anything else,
/// like `j/k`, stay inert. Reading the cells follows whatever wrapping the
/// renderer did.
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

/// Hit rects for an already drawn hint row whose hints are separated by two
/// or more spaces: `[L] Local    [Enter] confirm`, `[Esc close]  [S stop]`
/// or `R: restart  Esc: close`. The leading token, stripped of brackets and a
/// trailing colon, is the key, as in [`scan_dot_hints`].
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
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    #[test]
    fn clicks_press_the_hint_under_the_cursor_for_every_alignment() {
        let theme = load_theme("empire");
        let hints: &[Hint] = &[
            ("Enter", "select", KeyCode::Enter),
            ("Esc", "close", KeyCode::Esc),
        ];
        for (width, alignment) in [
            (40, Alignment::Left),
            (40, Alignment::Center),
            (41, Alignment::Center),
            (40, Alignment::Right),
        ] {
            let mut buttons = HintButtons::default();
            let mut terminal = Terminal::new(TestBackend::new(width, 1)).unwrap();
            terminal
                .draw(|f| buttons.render(f, f.area(), &theme, hints, alignment))
                .unwrap();
            let buf = terminal.backend().buffer().clone();
            let row: String = (0..width).map(|x| buf[(x, 0)].symbol()).collect();
            for (label, code) in [
                ("Enter select", KeyCode::Enter),
                ("Esc close", KeyCode::Esc),
            ] {
                let start = row.find(label).unwrap() as u16;
                let end = start + label.len() as u16 - 1;
                for x in [start, end] {
                    assert_eq!(
                        buttons.key_at(x, 0).map(|k| k.code),
                        Some(code),
                        "{alignment:?} width {width} col {x}"
                    );
                }
                assert!(buttons.key_at(end + 1, 0).map(|k| k.code) != Some(code));
            }
            assert!(buttons.handle_hover(row.find("Esc").unwrap() as u16, 0));
            assert!(buttons.handle_hover(0, 5));
        }
    }

    #[test]
    fn only_hints_that_fit_and_press_a_key_are_clickable() {
        let theme = load_theme("empire");
        let hints: &[Hint] = &[
            ("Enter", "select", KeyCode::Enter),
            ("Esc", "close", KeyCode::Esc),
        ];
        let mut buttons = HintButtons::default();
        let mut terminal = Terminal::new(TestBackend::new(14, 1)).unwrap();
        terminal
            .draw(|f| buttons.render(f, f.area(), &theme, hints, Alignment::Left))
            .unwrap();
        assert_eq!(buttons.targets.len(), 1);
        assert_eq!(buttons.key_at(13, 0), None);

        let inert: &[Hint] = &[
            ("Space", "toggle", KeyCode::Null),
            ("Esc", "close", KeyCode::Esc),
        ];
        terminal
            .draw(|f| buttons.render(f, f.area(), &theme, inert, Alignment::Left))
            .unwrap();
        assert_eq!(buttons.key_at(0, 0), None, "a Null hint is display-only");
    }

    #[test]
    fn dot_hints_map_each_drawn_segment_to_its_key() {
        let text = "enter view · ctrl+s save · j/k scroll · A always · esc close";
        let mut terminal = Terminal::new(TestBackend::new(40, 2)).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    Paragraph::new(text).wrap(ratatui::widgets::Wrap { trim: true }),
                    f.area(),
                )
            })
            .unwrap();
        let hints = scan_dot_hints(terminal.backend().buffer(), Rect::new(0, 0, 40, 2));
        let got: Vec<(u16, u16, u16, KeyEvent)> =
            hints.iter().map(|(k, r)| (r.x, r.y, r.width, *k)).collect();
        assert_eq!(
            got,
            vec![
                (0, 0, 10, KeyEvent::from(KeyCode::Enter)),
                (
                    13,
                    0,
                    11,
                    KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)
                ),
                (0, 1, 8, KeyEvent::from(KeyCode::Char('A'))),
                (11, 1, 9, KeyEvent::from(KeyCode::Esc)),
            ],
            "j/k is inert and wrapping is followed"
        );
    }

    #[test]
    fn spaced_hints_accept_every_serve_style() {
        let rows = [
            "[←/→] choose    [L] Local    [Enter] confirm",
            "Elapsed: 5s    [Esc close]  [S stop]",
            "Tab: URL  ?: help  Esc: close",
        ];
        let mut terminal = Terminal::new(TestBackend::new(50, 3)).unwrap();
        terminal
            .draw(|f| {
                f.render_widget(
                    Paragraph::new(rows.iter().map(|r| Line::from(*r)).collect::<Vec<_>>()),
                    f.area(),
                )
            })
            .unwrap();
        let hints = scan_spaced_hints(terminal.backend().buffer(), Rect::new(0, 0, 50, 3));
        let got: Vec<(u16, u16, u16, KeyCode)> = hints
            .iter()
            .map(|(k, r)| (r.x, r.y, r.width, k.code))
            .collect();
        assert_eq!(
            got,
            vec![
                (16, 0, 9, KeyCode::Char('L')),
                (29, 0, 15, KeyCode::Enter),
                (15, 1, 11, KeyCode::Esc),
                (28, 1, 8, KeyCode::Char('S')),
                (0, 2, 8, KeyCode::Tab),
                (10, 2, 7, KeyCode::Char('?')),
                (19, 2, 10, KeyCode::Esc),
            ],
            "arrows and plain text stay inert"
        );
    }
}
