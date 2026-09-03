//! The preview pane's text view, its selection, and the cache behind it.

/// The output pane's text layout, captured at render time so the input
/// handlers (which run between frames) can map a screen cell to the
/// absolute content line beneath it and back. `pane` is the on-screen
/// rect the parsed agent output is painted into (the info header and
/// banner are already stripped off); `first_line` is the index of the
/// content line drawn on `pane`'s top row (i.e. `compute_scroll`'s
/// result for the current scroll offset); `total_lines` is the parsed
/// scrollback length. The output Paragraph renders with no wrap and no
/// horizontal scroll, so screen row `pane.y + k` shows content line
/// `first_line + k`, and screen col `pane.x + c` shows content column
/// `c`. A `total_lines` of 0 means "no selectable content" (creating /
/// no-selection / empty panes).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(in crate::tui) struct PreviewTextView {
    pub(in crate::tui) pane: ratatui::layout::Rect,
    pub(in crate::tui) first_line: usize,
    pub(in crate::tui) total_lines: usize,
}

impl PreviewTextView {
    /// True when `(col, row)` lands on a row/col that maps to real
    /// content. Used to gate drag-select start. Rows in the pane below
    /// the last painted line are rejected: `screen_to_content` clamps
    /// them onto the last line, so accepting one would anchor a
    /// selection on text the user never clicked.
    pub(in crate::tui) fn contains(self, col: u16, row: u16) -> bool {
        if !self
            .pane
            .contains(ratatui::layout::Position::from((col, row)))
        {
            return false;
        }
        let painted_rows = self.total_lines.saturating_sub(self.first_line);
        usize::from(row - self.pane.y) < painted_rows
    }

    /// Absolute parsed-text index of the line painted on screen row
    /// `row`, clamped into the pane and the scrollback.
    fn abs_line_at_row(self, row: u16) -> usize {
        let pane = self.pane;
        let max_y = pane.bottom().saturating_sub(1);
        let cy = row.clamp(pane.y, max_y);
        let mut line = self.first_line + (cy - pane.y) as usize;
        if self.total_lines > 0 {
            line = line.min(self.total_lines - 1);
        }
        line
    }

    /// Map a screen cell to selection coords `(col_offset, from_bottom)`,
    /// clamped into the pane and the scrollback. `col_offset` is 0-based
    /// from the pane's left edge; `from_bottom` counts lines up from the
    /// newest captured line (0 = the bottom line). See `PreviewSelection`
    /// for why selections anchor to the bottom rather than an absolute
    /// index.
    pub(in crate::tui) fn screen_to_content(self, col: u16, row: u16) -> (u16, usize) {
        let pane = self.pane;
        let max_x = pane.right().saturating_sub(1);
        let col_off = col.clamp(pane.x, max_x) - pane.x;
        let abs = self.abs_line_at_row(row);
        (
            col_off,
            self.total_lines.saturating_sub(1).saturating_sub(abs),
        )
    }

    /// Absolute parsed-text index for a `from_bottom` distance under this
    /// view's current `total_lines`. The inverse of the `from_bottom` term
    /// in `screen_to_content`.
    fn abs_from_bottom(self, from_bottom: usize) -> usize {
        self.total_lines
            .saturating_sub(1)
            .saturating_sub(from_bottom)
    }
}

/// Flow-style text selection in the preview pane, matching tmux's
/// default mouse selection: from the anchor cell, the selection runs in
/// reading order (left-to-right, top-to-bottom) wrapping across every
/// row in between, and ends at the extent cell.
///
/// Coordinates are *content* coords, not screen cells: `col` is a 0-based
/// offset from the output pane's left edge and `from_bottom` counts lines
/// up from the newest captured line (0 = the bottom line). Anchoring to
/// the bottom (not an absolute index) is load-bearing: in live mode the
/// preview re-captures every frame, and the captured window *grows from
/// the top* as the user scrolls back (`capture_lines_for` adds the scroll
/// offset), so an absolute index would silently point at an older line as
/// the window grew, the exact bug where a drag-to-scroll copied the wrong
/// range. Distance from the newest line is invariant under that top-side
/// growth, so the highlight and the copy stay locked to the same text as
/// the user scrolls. The renderer re-derives screen rects each frame from
/// the live `PreviewTextView` via `screen_flow_rects`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::tui) struct PreviewSelection {
    /// Content cell the user pressed Down(Left) on: `(col_offset, from_bottom)`.
    pub(in crate::tui) anchor: (u16, usize),
    /// Current (or final) extent. Equals `anchor` at drag start.
    pub(in crate::tui) extent: (u16, usize),
    /// True once Up(Left) has fired. The renderer keeps the highlight
    /// visible after release until the user dismisses it (next key or
    /// click), so they can verify what was copied.
    pub(in crate::tui) finalized: bool,
}

impl PreviewSelection {
    /// Anchor and extent resolved to absolute parsed-text indices under
    /// `total_lines` and ordered in reading order (line first, then
    /// column). The first tuple is where the selection starts in the flow;
    /// the second is where it ends. A drag that runs up-and-right still
    /// resolves to the higher line as the start.
    pub(in crate::tui) fn ordered_abs(self, view: PreviewTextView) -> ((u16, usize), (u16, usize)) {
        let (ac, ad) = self.anchor;
        let (ec, ed) = self.extent;
        let a = (ac, view.abs_from_bottom(ad));
        let e = (ec, view.abs_from_bottom(ed));
        if (a.1, a.0) <= (e.1, e.0) {
            (a, e)
        } else {
            (e, a)
        }
    }

    /// Decompose the selection into per-row flow-shape screen `Rect`s,
    /// clipped to the visible window described by `view`. Lines above or
    /// below the visible window are skipped (the highlight just doesn't
    /// paint there); a partially-visible multi-line selection runs to the
    /// pane's right edge on every row but its last and from the left edge
    /// on every row but its first, matching the tmux default flow shape.
    /// Returns an empty vec when the pane is zero-sized.
    pub(in crate::tui) fn screen_flow_rects(
        self,
        view: PreviewTextView,
    ) -> Vec<ratatui::layout::Rect> {
        let pane = view.pane;
        let mut out = Vec::new();
        if pane.width == 0 || pane.height == 0 {
            return out;
        }
        let ((start_col, start_line), (end_col, end_line)) = self.ordered_abs(view);
        let top = view.first_line;
        let bottom_excl = top + pane.height as usize;
        for line in start_line..=end_line {
            if line < top || line >= bottom_excl {
                continue;
            }
            let row = pane.y + (line - top) as u16;
            let left_off = if line == start_line { start_col } else { 0 };
            let right_off_excl = if line == end_line {
                end_col.saturating_add(1).min(pane.width)
            } else {
                pane.width
            };
            let left = pane.x + left_off.min(pane.width);
            let right_excl = pane.x + right_off_excl;
            if right_excl > left {
                out.push(ratatui::layout::Rect {
                    x: left,
                    y: row,
                    width: right_excl - left,
                    height: 1,
                });
            }
        }
        out
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
    /// Cursor and terminal mode flags from the same accepted capture frame as
    /// content. Paint and input routing must never read a newer worker sample.
    pub(in crate::tui) cursor: Option<crate::tmux::PaneCursor>,
    /// Number of lines that were captured into `content`. Used together with
    /// the BUFFER reserve so consecutive wheel ticks don't trigger a fresh
    /// `tmux capture-pane` subprocess while the cached window still covers
    /// the requested scroll.
    pub(in crate::tui) captured_lines: usize,
    /// Lazily parsed ratatui `Text` view of `content`. Populated on the
    /// first render after a refresh that wasn't a no-op; reused as-is
    /// on every subsequent render until `content` is replaced. The
    /// invalidation point is `apply_worker_capture`, which sets this to
    /// `None` whenever it writes fresh content. See
    /// `PreviewCache::ensure_parsed` for the lazy-parse contract.
    ///
    /// Without this cache, `ansi-to-tui` re-parses the full pane
    /// payload (~12 KB of ANSI text for a typical agent) on every
    /// render iteration, including the many that fire on ticker
    /// wake-ups or unrelated key events. With it, the parse happens
    /// at most once per actual content change.
    pub(in crate::tui) parsed_text: Option<ratatui::text::Text<'static>>,
}

impl PreviewCache {
    /// Ensure `parsed_text` is populated, parsing `content` if it is
    /// not already cached. Side-effect only: returns nothing so the
    /// caller can drop the `&mut` borrow before reading
    /// `parsed_text` (which lets shared borrows on sibling fields of
    /// the parent struct coexist with the read).
    ///
    /// Cheap on cache-hit (single `is_none` check). Cache-miss runs
    /// `parse_output_text` once and stashes the result.
    pub(in crate::tui) fn ensure_parsed(&mut self) {
        if self.content.is_empty() {
            self.parsed_text = None;
            return;
        }
        if self.parsed_text.is_none() {
            self.parsed_text = Some(crate::tui::components::preview::parse_output_text(
                &self.content,
            ));
        }
    }

    /// Store a fresh capture, invalidating the parsed cache and stamping
    /// the session, target, generation, and dimensions the content belongs to.
    /// Returns the captured line count so the caller can clamp scroll. Written
    /// only by `apply_worker_capture`; there is no synchronous capture source.
    /// Whether no frame has landed yet for the displayed session `id`, so
    /// the render paints nothing rather than a hint about an unknown pane.
    pub(in crate::tui) fn is_pending_for(&self, id: &str) -> bool {
        self.session_id.as_deref() != Some(id)
    }

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
        // Invalidate the cached parse; the next `ensure_parsed` re-runs
        // `ansi-to-tui`.
        self.parsed_text = None;
        self.session_id = Some(session_id);
        self.capture_target = Some(capture_target);
        self.capture_generation = capture_generation;
        self.dimensions = dimensions;
        self.cursor = cursor;
        self.captured_lines
    }
}

/// Per-frame durations for the preview pipeline's paint-side apply/parse phases.
/// Lives on `HomeView`, resets each frame in `App::render`, and feeds the render
/// sampler so slow frames distinguish mailbox/cache application from ANSI
/// parsing and widget construction. Actual tmux capture runs on the worker.
#[derive(Default, Clone, Copy)]
pub(in crate::tui) struct PreviewTimings {
    pub(in crate::tui) apply: std::time::Duration,
    pub(in crate::tui) parse: std::time::Duration,
}
