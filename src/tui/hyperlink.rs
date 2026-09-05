//! A ratatui backend that carries OSC 8 hyperlinks through to the terminal.
//!
//! ratatui has no hyperlink cell attribute, so a link's target cannot ride
//! along inside the `Buffer`. `Backend::draw` does hand the writer `(x, y)` per
//! cell though, which is enough: the renderer records which cells carry a
//! target in [`FrameHyperlinks`], and this backend splits the frame into runs
//! by target, wrapping each in `OSC 8`. The host terminal then owns the link
//! (hover feedback, its own open gesture) on top of aoe's own click handling.
//!
//! The hyperlink state machine here (emit only on change, reset to a known
//! state at frame start, sanitize the URI) is derived from herdr's
//! `src/protocol/render_ansi.rs`:
//!
//!   <https://github.com/herdrdev/herdr>
//!   Copyright the herdr authors, licensed under the Apache License 2.0
//!   (see `licenses/Apache-2.0.txt`).
//!
//! Changes from the original: herdr blits whole frames from its own wire
//! format, where each cell carries a hyperlink index, and it owns the writer
//! outright. This wraps ratatui's `CrosstermBackend` instead and looks the
//! target up by cell position from a side map, because aoe's grid comes from
//! `vt100`, which does not model hyperlinks per cell. See
//! `THIRD_PARTY_NOTICES.md`.

use std::collections::HashMap;
use std::io::{self, Write};

use ratatui::backend::{Backend, ClearType, CrosstermBackend, WindowSize};
use ratatui::buffer::Cell;
use ratatui::layout::{Position, Size};

/// Opens a hyperlink run. The empty-target form closes one.
const OSC8_CLOSE: &[u8] = b"\x1b]8;;\x1b\\";

/// Cells carrying an OSC 8 target for the frame being painted.
///
/// Targets are held in a table and referenced by index so a link spanning many
/// cells stores its URI once, matching herdr's `FrameData::hyperlinks`.
#[derive(Debug, Default)]
pub struct FrameHyperlinks {
    uris: Vec<String>,
    cells: HashMap<(u16, u16), usize>,
}

impl FrameHyperlinks {
    /// Drop the previous frame's cells. Called once per frame before any
    /// widget records into it, so a link that scrolled away does not linger.
    pub fn clear(&mut self) {
        self.uris.clear();
        self.cells.clear();
    }

    /// Record that the cell at `(x, y)` links to `uri`.
    ///
    /// Control bytes are stripped rather than escaped: a raw ESC or BEL in the
    /// payload would terminate the sequence early and let pane output inject
    /// arbitrary escapes into aoe's own output stream.
    pub fn insert(&mut self, x: u16, y: u16, uri: &str) {
        let sanitized: String = uri.chars().filter(|c| !c.is_control()).collect();
        if sanitized.is_empty() {
            return;
        }
        let index = match self.uris.iter().position(|held| *held == sanitized) {
            Some(index) => index,
            None => {
                self.uris.push(sanitized);
                self.uris.len() - 1
            }
        };
        self.cells.insert((x, y), index);
    }

    #[cfg(test)]
    fn get(&self, x: u16, y: u16) -> Option<&str> {
        let index = *self.cells.get(&(x, y))?;
        self.uris.get(index).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// Shared with the renderer, which fills it while painting the frame that this
/// backend is about to write.
pub type SharedHyperlinks = std::sync::Arc<std::sync::Mutex<FrameHyperlinks>>;

/// Wraps [`CrosstermBackend`], emitting OSC 8 around the cells that carry a
/// target. Every other backend method delegates untouched.
pub struct HyperlinkBackend<W: Write> {
    inner: CrosstermBackend<W>,
    links: SharedHyperlinks,
}

impl<W: Write> HyperlinkBackend<W> {
    pub fn new(writer: W, links: SharedHyperlinks) -> Self {
        Self {
            inner: CrosstermBackend::new(writer),
            links,
        }
    }
}

impl<W: Write> Write for HyperlinkBackend<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        Write::flush(&mut self.inner)
    }
}

impl<W: Write> Backend for HyperlinkBackend<W> {
    type Error = io::Error;

    fn draw<'a, I>(&mut self, content: I) -> io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a Cell)>,
    {
        let links = match self.links.lock() {
            Ok(links) => links,
            // A poisoned map only costs hyperlinks, never the frame.
            Err(poisoned) => poisoned.into_inner(),
        };
        if links.is_empty() {
            return self.inner.draw(content);
        }

        // Start from a known state: an interrupted write, or an active
        // hyperlink in the host terminal, must not bleed onto unlinked cells.
        self.inner.write_all(OSC8_CLOSE)?;

        // `CrosstermBackend::draw` keeps no state between calls (it re-derives
        // colors and cursor position each time and resets SGR at the end), so
        // a frame can be handed to it one run at a time.
        let mut run: Vec<(u16, u16, &Cell)> = Vec::new();
        let mut active: Option<usize> = None;
        for (x, y, cell) in content {
            let target = links.cells.get(&(x, y)).copied();
            if target != active {
                if !run.is_empty() {
                    self.inner.draw(run.drain(..))?;
                }
                if active.is_some() {
                    self.inner.write_all(OSC8_CLOSE)?;
                }
                if let Some(uri) = target.and_then(|index| links.uris.get(index)) {
                    write!(self.inner, "\x1b]8;;{uri}\x1b\\")?;
                }
                active = target;
            }
            run.push((x, y, cell));
        }
        if !run.is_empty() {
            self.inner.draw(run.drain(..))?;
        }
        if active.is_some() {
            self.inner.write_all(OSC8_CLOSE)?;
        }
        Ok(())
    }

    fn hide_cursor(&mut self) -> io::Result<()> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> io::Result<()> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> io::Result<Position> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<Position>>(&mut self, position: P) -> io::Result<()> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> io::Result<()> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ClearType) -> io::Result<()> {
        self.inner.clear_region(clear_type)
    }

    fn append_lines(&mut self, n: u16) -> io::Result<()> {
        self.inner.append_lines(n)
    }

    fn size(&self) -> io::Result<Size> {
        self.inner.size()
    }

    fn window_size(&mut self) -> io::Result<WindowSize> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> io::Result<()> {
        Backend::flush(&mut self.inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    /// A writer the test keeps a handle on, since `CrosstermBackend` does not
    /// lend its own back out.
    #[derive(Clone, Default)]
    struct Tap(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Tap {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Paint `cells` (x, y, symbol) through the backend and return the bytes it
    /// wrote, with ESC rendered readably.
    fn render(links: &[(u16, u16, &str)], cells: &[(u16, u16, &str)]) -> String {
        let shared: SharedHyperlinks = SharedHyperlinks::default();
        {
            let mut guard = shared.lock().unwrap();
            for (x, y, uri) in links {
                guard.insert(*x, *y, uri);
            }
        }
        let tap = Tap::default();
        let mut backend = HyperlinkBackend::new(tap.clone(), shared);
        let owned: Vec<(u16, u16, Cell)> = cells
            .iter()
            .map(|(x, y, s)| {
                let mut cell = Cell::default();
                cell.set_symbol(s);
                cell.fg = Color::Reset;
                (*x, *y, cell)
            })
            .collect();
        backend
            .draw(owned.iter().map(|(x, y, c)| (*x, *y, c)))
            .unwrap();
        let written = tap.0.lock().unwrap().clone();
        String::from_utf8_lossy(&written).replace('\u{1b}', "^[")
    }

    #[test]
    fn wraps_only_the_linked_cells() {
        let out = render(
            &[(1, 0, "https://example.com"), (2, 0, "https://example.com")],
            &[(0, 0, "a"), (1, 0, "b"), (2, 0, "c"), (3, 0, "d")],
        );
        // The run opens before `b`, and closes before `d` is written.
        let open = out
            .find("^[]8;;https://example.com^[\\")
            .expect("open emitted");
        let b = out.find('b').expect("b written");
        let c = out.find('c').expect("c written");
        let d = out.find('d').expect("d written");
        assert!(open < b, "link must open before its first cell");
        assert!(c < d, "cells stay in order");
        let close_after = out[c..d].find("^[]8;;^[\\");
        assert!(
            close_after.is_some(),
            "link must close between its last cell and the next unlinked one: {out}"
        );
    }

    /// The URI lives outside the `Buffer`, so ratatui's diff cannot see a
    /// target change on cells whose text and style are unchanged. Without the
    /// renderer marking linked cells `AlwaysUpdate`, the second frame here
    /// would emit nothing and the terminal would keep pointing at A.
    #[test]
    fn a_retargeted_cell_is_re_emitted_on_the_next_frame() {
        use ratatui::buffer::CellDiffOption;

        let shared: SharedHyperlinks = SharedHyperlinks::default();
        let tap = Tap::default();
        let mut terminal =
            ratatui::Terminal::new(HyperlinkBackend::new(tap.clone(), shared.clone()))
                .expect("terminal");

        let mut frame = |uri: &str| {
            {
                let mut guard = shared.lock().unwrap();
                guard.clear();
                guard.insert(0, 0, uri);
            }
            terminal
                .draw(|f| {
                    let mut cell = Cell::default();
                    cell.set_symbol("L");
                    cell.set_diff_option(CellDiffOption::AlwaysUpdate);
                    f.buffer_mut()[(0, 0)] = cell;
                })
                .expect("draw");
            let out =
                String::from_utf8_lossy(&tap.0.lock().unwrap().clone()).replace('\u{1b}', "^[");
            tap.0.lock().unwrap().clear();
            out
        };

        let first = frame("https://a.example");
        assert!(
            first.contains("^[]8;;https://a.example^[\\"),
            "first frame: {first}"
        );

        // Identical symbol and style, different target.
        let second = frame("https://b.example");
        assert!(
            second.contains("^[]8;;https://b.example^[\\"),
            "a retargeted cell must be re-emitted, got: {second}"
        );
        assert!(
            !second.contains("https://a.example"),
            "the old target must not survive: {second}"
        );
    }

    #[test]
    fn a_frame_with_no_links_emits_no_osc8() {
        let out = render(&[], &[(0, 0, "a"), (1, 0, "b")]);
        assert!(
            !out.contains("]8;"),
            "unlinked frame must stay clean: {out}"
        );
    }

    #[test]
    fn resets_inherited_link_state_before_painting() {
        let out = render(&[(0, 0, "https://example.com")], &[(0, 0, "a")]);
        assert!(
            out.starts_with("^[]8;;^[\\"),
            "frame must open from a known OSC 8 state: {out}"
        );
    }

    #[test]
    fn adjacent_runs_with_different_targets_each_get_their_own_sequence() {
        let out = render(
            &[(0, 0, "https://a.example"), (1, 0, "https://b.example")],
            &[(0, 0, "a"), (1, 0, "b")],
        );
        assert!(out.contains("^[]8;;https://a.example^[\\"));
        assert!(out.contains("^[]8;;https://b.example^[\\"));
    }

    #[test]
    fn control_bytes_in_a_target_are_stripped() {
        // A raw ESC would terminate the sequence and let pane output inject
        // escapes into aoe's own stream.
        let mut links = FrameHyperlinks::default();
        links.insert(0, 0, "https://example.com/\x1b]8;;evil\x07");
        assert_eq!(links.get(0, 0), Some("https://example.com/]8;;evil"));
        // A target that is nothing but control bytes records no cell at all.
        links.insert(1, 0, "\x1b\x07");
        assert_eq!(links.get(1, 0), None);
    }

    #[test]
    fn repeated_targets_share_one_table_entry() {
        let mut links = FrameHyperlinks::default();
        links.insert(0, 0, "https://example.com");
        links.insert(1, 0, "https://example.com");
        links.insert(2, 0, "https://other.example");
        assert_eq!(links.uris.len(), 2);
        assert_eq!(links.get(0, 0), links.get(1, 0));
    }
}
