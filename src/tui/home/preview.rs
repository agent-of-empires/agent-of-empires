//! The preview pane's text view, its selection, and the cache behind it.

use std::collections::HashSet;

use ratatui::layout::{Position, Rect};

/// The output pane's text layout, captured at render time so input handlers can map screen
/// cells to content lines. The output renders unwrapped and unscrolled horizontally, so row
/// `pane.y + k` shows line `first_line + k` and col `pane.x + c` shows column `c`.
/// `total_lines == 0` means nothing is selectable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tui) struct PreviewTextView {
    pub(in crate::tui) pane: Rect,
    pub(in crate::tui) first_line: usize,
    pub(in crate::tui) total_lines: usize,
}

impl PreviewTextView {
    /// True when `(col, row)` lands on painted content. Rows below the last painted line are
    /// rejected: `screen_to_content` would clamp them onto the last line and anchor a
    /// selection on text the user never clicked.
    pub(in crate::tui) fn contains(self, col: u16, row: u16) -> bool {
        self.pane.contains(Position::from((col, row)))
            && usize::from(row - self.pane.y) < self.total_lines.saturating_sub(self.first_line)
    }

    /// Absolute line index painted on screen `row`, clamped into the pane and scrollback.
    pub(in crate::tui) fn abs_line_at_row(self, row: u16) -> usize {
        let pane = self.pane;
        let cy = row.clamp(pane.y, pane.bottom().saturating_sub(1));
        let line = self.first_line + (cy - pane.y) as usize;
        if self.total_lines > 0 {
            line.min(self.total_lines - 1)
        } else {
            line
        }
    }

    /// Map a screen cell to clamped selection coords `(col_offset, from_bottom)`.
    pub(in crate::tui) fn screen_to_content(self, col: u16, row: u16) -> (u16, usize) {
        let pane = self.pane;
        let col_off = col.clamp(pane.x, pane.right().saturating_sub(1)) - pane.x;
        (col_off, self.abs_from_bottom(self.abs_line_at_row(row)))
    }

    /// Converts between an absolute index and a `from_bottom` distance (its own inverse).
    fn abs_from_bottom(self, n: usize) -> usize {
        self.total_lines.saturating_sub(1).saturating_sub(n)
    }
}

/// Flow-style text selection matching tmux's default mouse selection.
///
/// Coordinates are `(col_offset, from_bottom)`, counted up from the newest captured line.
/// Anchoring to the bottom is load-bearing: the captured window grows from the top as the
/// user scrolls back, so an absolute index would drift onto older lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) struct PreviewSelection {
    pub(in crate::tui) anchor: (u16, usize),
    pub(in crate::tui) extent: (u16, usize),
    /// Set on mouse release; the highlight stays until dismissed.
    pub(in crate::tui) finalized: bool,
}

impl PreviewSelection {
    /// Anchor and extent as absolute `(col, line)` under `view`, in reading order.
    pub(in crate::tui) fn ordered_abs(self, view: PreviewTextView) -> ((u16, usize), (u16, usize)) {
        let a = (self.anchor.0, view.abs_from_bottom(self.anchor.1));
        let e = (self.extent.0, view.abs_from_bottom(self.extent.1));
        if (a.1, a.0) <= (e.1, e.0) {
            (a, e)
        } else {
            (e, a)
        }
    }

    /// Per-row flow-shape screen rects clipped to the visible window: inner rows span the
    /// full pane width, the first starts at the start column and the last ends at the end
    /// column.
    pub(in crate::tui) fn screen_flow_rects(self, view: PreviewTextView) -> Vec<Rect> {
        let pane = view.pane;
        if pane.width == 0 || pane.height == 0 {
            return Vec::new();
        }
        let ((start_col, start_line), (end_col, end_line)) = self.ordered_abs(view);
        let top = view.first_line;
        let bottom_excl = top + pane.height as usize;
        (start_line.max(top)..=end_line.min(bottom_excl.saturating_sub(1)))
            .filter_map(|line| {
                let left_off = if line == start_line { start_col } else { 0 };
                let right_off_excl = if line == end_line {
                    end_col.saturating_add(1).min(pane.width)
                } else {
                    pane.width
                };
                let left = pane.x + left_off.min(pane.width);
                let right_excl = pane.x + right_off_excl;
                (right_excl > left).then(|| Rect {
                    x: left,
                    y: pane.y + (line - top) as u16,
                    width: right_excl - left,
                    height: 1,
                })
            })
            .collect()
    }
}

/// Cached preview content received from the off-thread capture worker.
#[derive(Default)]
pub(in crate::tui) struct PreviewCache {
    pub(in crate::tui) session_id: Option<String>,
    pub(in crate::tui) capture_target: Option<String>,
    pub(in crate::tui) capture_generation: u64,
    pub(in crate::tui) content: String,
    pub(in crate::tui) dimensions: (u16, u16),
    /// From the same capture frame as `content`; never mix in a newer sample.
    pub(in crate::tui) cursor: Option<crate::tmux::PaneCursor>,
    pub(in crate::tui) captured_lines: usize,
    /// Parsed `content`, cleared by `store_capture` so ANSI parsing runs at
    /// most once per content change instead of on every render.
    pub(in crate::tui) parsed_text: Option<ratatui::text::Text<'static>>,
    /// OSC 8 targets for the pane, kept beside the text because neither the
    /// vt100 grid nor a ratatui cell can carry a hyperlink.
    pub(in crate::tui) links: Vec<crate::tmux::osc8::PaneLink>,
    /// Pane link generation `links` was collected at; a target can change
    /// while the rendered grid stays byte-identical.
    pub(in crate::tui) links_generation: u64,
}

impl PreviewCache {
    /// Populate `parsed_text` (and `links`) if stale. Returns nothing so the
    /// caller can drop the `&mut` borrow before reading the fields.
    pub(in crate::tui) fn ensure_parsed(&mut self) {
        if self.content.is_empty() {
            self.parsed_text = None;
            self.links.clear();
            return;
        }
        let generation = self
            .capture_target
            .as_deref()
            .map_or(0, crate::tmux::pane_links_generation);
        if self.parsed_text.is_none() {
            self.parsed_text = Some(crate::tui::components::preview::parse_output_text(
                &self.content,
            ));
        } else if generation == self.links_generation {
            return;
        }
        self.links = self.collect_links();
        self.links_generation = generation;
    }

    /// Links from the VT channel (which taps the raw stream, since the grid
    /// drops OSC 8) plus any still present in captured `content`.
    fn collect_links(&self) -> Vec<crate::tmux::osc8::PaneLink> {
        use crate::tmux::osc8;

        let mut links = self
            .capture_target
            .as_deref()
            .map(crate::tmux::pane_links)
            .unwrap_or_default();
        let from_channel = links.len();
        // Dedupe on (text, uri): two texts pointing at one URL are separate
        // clickable runs. Capped like the channel's own table.
        let mut seen: HashSet<(String, String)> = links
            .iter()
            .map(|l| (l.text.clone(), l.uri.clone()))
            .collect();
        for link in osc8::extract_links(self.content.as_bytes()) {
            if links.len() >= osc8::MAX_PANE_LINKS {
                break;
            }
            if seen.insert((link.text.clone(), link.uri.clone())) {
                links.push(link);
            }
        }
        let pane = self.capture_target.as_deref().unwrap_or("<none>");
        if links.is_empty() {
            if osc8::has_hyperlink(self.content.as_bytes()) {
                tracing::debug!(
                    target: "tui.preview_links",
                    pane,
                    bytes = self.content.len(),
                    "preview: frame carries OSC 8 but no link parsed"
                );
            }
        } else {
            tracing::debug!(
                target: "tui.preview_links",
                pane,
                from_channel,
                from_content = links.len() - from_channel,
                texts = ?links.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
                "preview: collected pane hyperlinks"
            );
        }
        links
    }

    /// Whether no frame has landed yet for session `id`.
    pub(in crate::tui) fn is_pending_for(&self, id: &str) -> bool {
        self.session_id.as_deref() != Some(id)
    }

    /// Store a fresh worker capture, invalidating the parse. Returns the captured line count.
    pub(in crate::tui) fn store_capture(
        &mut self,
        content: String,
        session_id: String,
        capture_target: String,
        capture_generation: u64,
        dimensions: (u16, u16),
        cursor: Option<crate::tmux::PaneCursor>,
    ) -> usize {
        self.captured_lines = content.lines().count();
        self.content = content;
        self.parsed_text = None;
        self.session_id = Some(session_id);
        self.capture_target = Some(capture_target);
        self.capture_generation = capture_generation;
        self.dimensions = dimensions;
        self.cursor = cursor;
        self.captured_lines
    }
}

/// Per-frame paint-side preview durations for the render sampler.
#[derive(Default, Clone, Copy)]
pub(in crate::tui) struct PreviewTimings {
    pub(in crate::tui) apply: std::time::Duration,
    pub(in crate::tui) parse: std::time::Duration,
}
