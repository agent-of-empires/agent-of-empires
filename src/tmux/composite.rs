//! Splice a tmux window's panes into one screen-shaped snapshot.
//!
//! `capture-pane` is per-pane and tmux has no command that returns a window
//! with its panes composited, so the preview historically showed only the
//! pinned `^.0` pane and a user split was invisible (see
//! [`crate::tmux::Session::capture_window_composited_with_cursor`] for the capture side).
//! This module is the pure half: given each pane's geometry and its captured
//! rows, lay them back out on the window grid and draw tmux-style borders in
//! the gaps between them.
//!
//! Compositing is read-only and changes nothing about input routing, which
//! stays pinned to `^.0` (#435, #488).
//!
//! Known residual: a window row covered by no pane (a `pane-border-status
//! top` status line, which tmux draws itself and no capture sees) is filled
//! with a full-width border rule. Cursor translation does not remove that row;
//! dropping it would render fewer than `window_height` rows and rebase every
//! consumer, so the rule stays.

/// One pane's rectangle within its window, from
/// `#{pane_left} #{pane_top} #{pane_width} #{pane_height}`. Public because
/// [`crate::tmux::PaneCursor::composite_pane0`] exposes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneGeom {
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
}

impl PaneGeom {
    /// Parse the four space-separated fields tmux emits for a pane's
    /// rectangle. Returns `None` for a malformed line so a single unparseable
    /// pane degrades that pane to border fill rather than failing the frame.
    pub(crate) fn parse(line: &str) -> Option<Self> {
        let mut f = line.split_whitespace();
        let left = f.next()?.parse().ok()?;
        let top = f.next()?.parse().ok()?;
        let width = f.next()?.parse().ok()?;
        let height = f.next()?.parse().ok()?;
        Some(Self {
            left,
            top,
            width,
            height,
        })
    }

    fn covers_row(&self, row: u16) -> bool {
        row >= self.top && row < self.top.saturating_add(self.height)
    }

    /// Whether two pane rectangles share any cell.
    ///
    /// Panes in a normal window tile the grid, but a zoomed pane (`C-b z`) is
    /// reported at the window's full rectangle while its neighbours keep their
    /// own, so they overlap. [`composite_window`]'s walk assumes a tiling, so
    /// [`crate::tmux::Session::capture_window_layout`] uses this to drop panes it
    /// cannot lay out rather than painting a scrambled frame.
    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        let x_overlap = self.left < other.left.saturating_add(other.width)
            && other.left < self.left.saturating_add(self.width);
        let y_overlap = self.top < other.top.saturating_add(other.height)
            && other.top < self.top.saturating_add(self.height);
        x_overlap && y_overlap
    }

    fn covers(&self, row: u16, col: u16) -> bool {
        self.covers_row(row) && col >= self.left && col < self.left.saturating_add(self.width)
    }
}

/// A pane's rectangle plus its rows, each already padded to `geom.width`
/// display columns by [`crate::tmux::vt::capture_rows_padded`].
pub(crate) struct CapturedPane {
    pub geom: PaneGeom,
    pub rows: Vec<String>,
}

/// A window's dimensions and every pane's rectangle plus captured rows.
///
/// Held as a unit so the live preview can cache one across frames: the panes
/// the user is only *watching* refresh on a lazy cadence, while pane 0 (the
/// one receiving input, whose latency is the only one that can be felt) is
/// re-rendered every frame from its VT grid.
pub(crate) struct WindowLayout {
    pub window_width: u16,
    pub window_height: u16,
    pub panes: Vec<CapturedPane>,
}

impl WindowLayout {
    pub(crate) fn composite(&self) -> String {
        composite_window(self.window_width, self.window_height, &self.panes)
    }

    /// The first pane's rectangle. Pane indices follow layout order, but the
    /// top-left pane can still have a non-zero origin when window chrome
    /// reserves rows or columns. Keep that origin as geometry, not an assumed
    /// `(0, 0)`.
    pub(crate) fn first_pane(&self) -> Option<PaneGeom> {
        self.panes.first().map(|p| p.geom)
    }

    /// Composite with the first pane's rows swapped for `rows`, for the live
    /// path's fresh VT-grid frame over a cached layout.
    pub(crate) fn composite_with_first_pane_rows(&self, rows: &[String]) -> String {
        let Some(first) = self.panes.first() else {
            return self.composite();
        };
        let mut panes: Vec<CapturedPane> = Vec::with_capacity(self.panes.len());
        panes.push(CapturedPane {
            geom: first.geom,
            rows: rows.to_vec(),
        });
        for pane in &self.panes[1..] {
            panes.push(CapturedPane {
                geom: pane.geom,
                rows: pane.rows.clone(),
            });
        }
        composite_window(self.window_width, self.window_height, &panes)
    }
}

/// Drop a composed row's trailing padding, which buys nothing and only risks
/// the renderer wrapping a row that is exactly the viewport width.
///
/// Trailing spaces are only padding when nothing is colouring them. The
/// rightmost pane's last row may legitimately end in a background fill running
/// to the window edge (a status bar, a selection), which arrives here as an SGR
/// followed by spaces; blanket-trimming those would strip the cells while
/// leaving the escape, silently shortening the fill.
/// [`crate::tmux::vt::capture_rows_padded`] treats a styled blank as content
/// for the same reason, so this keeps the two halves of the pipeline agreeing.
///
/// Padding this module and `capture_rows_padded` append is always introduced by
/// an explicit reset, so a reset immediately before the spaces is the signal
/// that they are safe to drop (along with the now-pointless reset). A row of
/// bare spaces carrying no escapes at all is a blank row and trims to nothing.
fn trim_padding(line: &str) -> &str {
    let trimmed = line.trim_end_matches(' ');
    if trimmed.len() == line.len() {
        return line;
    }
    if let Some(rest) = trimmed.strip_suffix(SGR_RESET) {
        return rest;
    }
    // Unstyled blanks: no escape anywhere, so there is nothing to preserve.
    if !trimmed.contains('\x1b') {
        return trimmed;
    }
    // Spaces under a live SGR: a coloured fill, not padding.
    line
}

/// The reset [`crate::tmux::vt::capture_rows_padded`] emits before padding a row
/// out to its pane's width.
const SGR_RESET: &str = "\x1b[0m";

/// Border glyphs. tmux draws proper tee/cross junctions; a preview only needs
/// the two edges, so a full-width gap row is drawn as an unbroken rule rather
/// than tracking which columns carry a vertical border through it.
const BORDER_VERTICAL: char = '│';
const BORDER_HORIZONTAL: char = '─';
/// Where a horizontal and a vertical rule cross. Reached only when no pane
/// touches the cell orthogonally but one touches it diagonally.
const BORDER_CROSS: char = '┼';

/// Lay `panes` back onto a `window_width` x `window_height` grid and return the
/// rows, each terminated by `\n`, ready to be handed to the preview cache like a
/// single-pane `capture-pane` result.
///
/// Every row is TERMINATED rather than joined, matching `capture-pane`, so the
/// result always counts `window_height` lines. Joining instead lost the last row
/// whenever it was blank (a stacked split with an idle shell underneath), because
/// `str::lines` and the renderer's ANSI parser both drop a trailing empty segment.
/// The cursor is rebased onto `window_height`, so a short count painted it a row
/// above the text, and only for some splits, since a side-by-side border glyph
/// makes the last row non-empty.
///
/// Walks each window row left to right, emitting a pane's row whole whenever
/// the cursor reaches that pane's left edge and a border glyph otherwise. Rows
/// are emitted whole, never sliced at a column, which is what keeps the
/// ANSI-laden content correct: slicing a styled row at a display column would
/// mean parsing SGR state mid-string.
///
/// A column the walk cannot attribute to any pane advances by one and is
/// filled, so a layout this function does not understand degrades to border
/// fill instead of panicking or dropping the frame.
pub(crate) fn composite_window(
    window_width: u16,
    window_height: u16,
    panes: &[CapturedPane],
) -> String {
    let mut out = String::new();
    for row in 0..window_height {
        // An unclaimed cell is a border only where it actually separates two
        // panes, and which glyph depends on the direction it separates them
        // in: a pane directly above or below makes it part of a horizontal
        // rule, a pane to the left or right makes it part of a vertical one.
        // A cell with no pane on any side is void (the dead corner beside a
        // short pane) and stays blank rather than drawing a border to nowhere,
        // UNLESS a pane touches it diagonally, which only happens where a
        // horizontal and a vertical rule cross. Those cells used to render as a
        // hole in the middle of an otherwise unbroken rule.
        let covered = |r: u16, c: u16| panes.iter().any(|p| p.geom.covers(r, c));
        let gap_fill = |col: u16| -> char {
            let up = row.checked_sub(1);
            let left = col.checked_sub(1);
            let down = row.saturating_add(1);
            let right = col.saturating_add(1);
            if up.is_some_and(|r| covered(r, col)) || covered(down, col) {
                BORDER_HORIZONTAL
            } else if left.is_some_and(|c| covered(row, c)) || covered(row, right) {
                BORDER_VERTICAL
            } else if up.zip(left).is_some_and(|(r, c)| covered(r, c))
                || up.is_some_and(|r| covered(r, right))
                || left.is_some_and(|c| covered(down, c))
                || covered(down, right)
            {
                BORDER_CROSS
            } else {
                ' '
            }
        };

        let mut line = String::new();
        let mut col = 0u16;
        // Whether the last thing written was pane content, whose SGR state may
        // still be live. `capture_rows_padded` only resets when it actually pads,
        // so a pane row whose fill runs to its own right edge leaves the colour
        // set and would paint the border glyph beside it in that background.
        let mut sgr_live = false;
        while col < window_width {
            let hit = panes
                .iter()
                .find(|p| p.geom.left == col && p.geom.covers_row(row));
            match hit {
                Some(pane) if pane.geom.width > 0 => {
                    if let Some(text) = pane.rows.get((row - pane.geom.top) as usize) {
                        line.push_str(text);
                        // A row that already ends in the padding reset needs no
                        // second one; only a fill running to the pane's own right
                        // edge leaves the colour set.
                        sgr_live = text.contains('\x1b') && !text.ends_with(SGR_RESET);
                    } else {
                        // Short capture (pane resized mid-frame): pad rather
                        // than shift every pane to its right.
                        line.extend(std::iter::repeat_n(' ', pane.geom.width as usize));
                    }
                    col = col.saturating_add(pane.geom.width);
                }
                _ => {
                    if sgr_live {
                        line.push_str(SGR_RESET);
                        sgr_live = false;
                    }
                    line.push(gap_fill(col));
                    col = col.saturating_add(1);
                }
            }
        }
        out.push_str(trim_padding(&line));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(left: u16, top: u16, width: u16, height: u16, rows: &[&str]) -> CapturedPane {
        CapturedPane {
            geom: PaneGeom {
                left,
                top,
                width,
                height,
            },
            rows: rows.iter().map(|r| r.to_string()).collect(),
        }
    }

    #[test]
    fn parses_a_geometry_line() {
        assert_eq!(
            PaneGeom::parse("0 0 80 24"),
            Some(PaneGeom {
                left: 0,
                top: 0,
                width: 80,
                height: 24
            })
        );
        assert_eq!(PaneGeom::parse("0 0 80"), None);
        assert_eq!(PaneGeom::parse("a b c d"), None);
        assert_eq!(PaneGeom::parse(""), None);
    }

    #[test]
    fn single_pane_fills_the_window_unchanged() {
        let panes = [pane(0, 0, 5, 2, &["hello", "world"])];
        assert_eq!(composite_window(5, 2, &panes), "hello\nworld\n");
    }

    #[test]
    fn side_by_side_panes_are_joined_by_a_vertical_border() {
        // `C-b %`: two panes split at column 5, tmux's border occupying it.
        let panes = [
            pane(0, 0, 5, 2, &["aaaaa", "bbbbb"]),
            pane(6, 0, 5, 2, &["ccccc", "ddddd"]),
        ];
        assert_eq!(
            composite_window(11, 2, &panes),
            "aaaaa│ccccc\nbbbbb│ddddd\n"
        );
    }

    #[test]
    fn stacked_panes_are_joined_by_a_horizontal_border() {
        // `C-b "`: the row between the panes belongs to no pane.
        let panes = [pane(0, 0, 4, 1, &["topp"]), pane(0, 2, 4, 1, &["botm"])];
        assert_eq!(composite_window(4, 3, &panes), "topp\n────\nbotm\n");
    }

    #[test]
    fn a_pane_shorter_than_its_neighbour_pads_rather_than_shifting() {
        // The right pane spans both rows; the left only the first. Row 1 must
        // still start the right pane at the same column, and the space the
        // short pane vacated reads as the rule tmux would draw beneath it.
        let panes = [
            pane(0, 0, 3, 1, &["abc"]),
            pane(4, 0, 3, 2, &["xyz", "uvw"]),
        ];
        assert_eq!(composite_window(7, 2, &panes), "abc│xyz\n───│uvw\n");
    }

    #[test]
    fn a_row_missing_from_a_capture_pads_its_width() {
        // Pane claims two rows but the capture came back with one (a resize
        // raced the frame). The neighbour must not slide left.
        let panes = [
            pane(0, 0, 3, 2, &["abc"]),
            pane(4, 0, 3, 2, &["xyz", "uvw"]),
        ];
        assert_eq!(composite_window(7, 2, &panes), "abc│xyz\n   │uvw\n");
    }

    fn layout(w: u16, h: u16, panes: Vec<CapturedPane>) -> WindowLayout {
        WindowLayout {
            window_width: w,
            window_height: h,
            panes,
        }
    }

    #[test]
    fn first_pane_is_the_one_at_the_window_origin() {
        // tmux layout order puts pane 0 at the top-left pane position; its
        // actual origin remains part of the returned geometry.
        let l = layout(
            9,
            1,
            vec![pane(0, 0, 4, 1, &["left"]), pane(5, 0, 4, 1, &["rght"])],
        );
        let first = l.first_pane().expect("a first pane");
        assert_eq!((first.left, first.top), (0, 0));
    }

    #[test]
    fn swapping_the_first_pane_rows_leaves_the_others_alone() {
        // The live path's whole trick: a cached layout re-rendered with only
        // pane 0 refreshed from its VT grid.
        let l = layout(
            9,
            2,
            vec![
                pane(0, 0, 4, 2, &["old1", "old2"]),
                pane(5, 0, 4, 2, &["keep", "same"]),
            ],
        );
        let fresh = vec!["new1".to_string(), "new2".to_string()];
        assert_eq!(
            l.composite_with_first_pane_rows(&fresh),
            "new1│keep\nnew2│same\n"
        );
        // The cached layout is not consumed: the next frame swaps again.
        assert_eq!(l.composite(), "old1│keep\nold2│same\n");
    }

    /// A composite must always count `window_height` lines, whatever the bottom
    /// row holds. The cursor is rebased onto `window_height`, so a row lost off
    /// the bottom paints it one row too high, and a blank bottom row is the
    /// common case (a stacked split with an idle shell underneath).
    #[test]
    fn every_row_is_terminated_so_the_line_count_matches_the_window() {
        for (label, panes) in [
            ("blank bottom row", vec![pane(0, 0, 4, 1, &["top."])]),
            (
                "content on every row",
                vec![pane(0, 0, 4, 3, &["r0..", "r1..", "r2.."])],
            ),
            ("no panes at all", vec![]),
        ] {
            let out = composite_window(4, 3, &panes);
            assert_eq!(out.lines().count(), 3, "{label}: lines() short");
            assert!(out.ends_with('\n'), "{label}: last row not terminated");
        }
    }

    /// A zoomed pane (`C-b z`) is reported at the window's full rectangle while
    /// its neighbours keep theirs, so the rectangles OVERLAP and the walk's
    /// tiling assumption breaks: it painted one pane then filled the rest of
    /// every row with border glyphs, hiding the zoomed pane entirely. The capture
    /// side drops overlapping panes; this pins the geometry test it relies on.
    #[test]
    fn overlapping_rectangles_are_detected() {
        // The real measured zoom layout: 40x8 window split at column 20, then
        // pane 1 zoomed to the full window.
        let unzoomed_0 = PaneGeom {
            left: 0,
            top: 0,
            width: 20,
            height: 8,
        };
        let unzoomed_1 = PaneGeom {
            left: 21,
            top: 0,
            width: 19,
            height: 8,
        };
        let zoomed_1 = PaneGeom {
            left: 0,
            top: 0,
            width: 40,
            height: 8,
        };
        assert!(
            !unzoomed_0.overlaps(&unzoomed_1),
            "a normal split tiles and must not be dropped"
        );
        assert!(unzoomed_0.overlaps(&zoomed_1), "zoomed pane must be caught");
        assert!(zoomed_1.overlaps(&unzoomed_0), "overlap is symmetric");
        // Stacked panes separated by a rule row also tile.
        let top = PaneGeom {
            left: 0,
            top: 0,
            width: 9,
            height: 1,
        };
        let bottom = PaneGeom {
            left: 0,
            top: 2,
            width: 9,
            height: 1,
        };
        assert!(!top.overlaps(&bottom));
        // A zero-width pane touches nothing.
        let empty = PaneGeom {
            left: 0,
            top: 0,
            width: 0,
            height: 8,
        };
        assert!(!empty.overlaps(&unzoomed_0));
    }

    /// Where a horizontal and a vertical rule cross, no pane touches the cell
    /// orthogonally, so it used to render as a blank hole in the middle of an
    /// otherwise unbroken rule. A pane on the diagonal marks it as a junction.
    #[test]
    fn a_rule_crossing_draws_a_junction_not_a_hole() {
        // Four panes in a 2x2 grid, rules at row 1 and column 4.
        let panes = [
            pane(0, 0, 4, 1, &["tl.."]),
            pane(5, 0, 4, 1, &["tr.."]),
            pane(0, 2, 4, 1, &["bl.."]),
            pane(5, 2, 4, 1, &["br.."]),
        ];
        let out = composite_window(9, 3, &panes);
        let rule = out.lines().nth(1).expect("rule row");
        assert_eq!(rule, "────┼────", "cross cell should be a junction");
    }

    /// The dead corner beside a SHORT pane has no pane on any side and no pane
    /// on the diagonal either, so it must stay blank rather than being promoted
    /// to a junction by the crossing rule above.
    #[test]
    fn a_dead_corner_stays_blank() {
        // A one-row pane on the left, a three-row pane on the right. Rows 1-2 of
        // the left column are void.
        let panes = [
            pane(0, 0, 3, 1, &["abc"]),
            pane(4, 0, 3, 3, &["x", "y", "z"]),
        ];
        let out = composite_window(7, 3, &panes);
        let last = out.lines().nth(2).expect("row 2");
        assert!(
            !last.contains('┼'),
            "void corner became a junction: {last:?}"
        );
    }

    #[test]
    fn swapping_rows_on_an_empty_layout_is_a_no_op() {
        let l = layout(3, 1, vec![]);
        assert_eq!(l.composite_with_first_pane_rows(&["x".to_string()]), "\n");
    }

    #[test]
    fn a_left_column_split_in_two_draws_a_rule_between_its_panes() {
        // The layout `C-b %` then `C-b "` produces: a tall pane down the right
        // side, and the left column split into two stacked panes. The rule
        // between the stacked pair must stop at the vertical border rather
        // than running through it or vanishing.
        let panes = [
            pane(0, 0, 4, 1, &["top1"]),
            pane(0, 2, 4, 1, &["bot1"]),
            pane(5, 0, 4, 3, &["rgt1", "rgt2", "rgt3"]),
        ];
        assert_eq!(
            composite_window(9, 3, &panes),
            "top1│rgt1\n────│rgt2\nbot1│rgt3\n"
        );
    }

    #[test]
    fn ansi_rows_are_spliced_without_being_cut() {
        // A styled row must arrive at the seam intact, escapes and all.
        let left = "\x1b[0m\x1b[31mred\x1b[0m";
        let right = "\x1b[0m\x1b[32mgrn\x1b[0m";
        let panes = [pane(0, 0, 3, 1, &[left]), pane(4, 0, 3, 1, &[right])];
        let out = composite_window(7, 1, &panes);
        assert_eq!(out, format!("{left}│{right}\n"));
    }

    #[test]
    fn an_unclaimed_column_degrades_to_border_fill() {
        // A layout the walk cannot attribute (pane starts at 2, nothing at 0)
        // must still produce a full-width row instead of looping or panicking.
        // Only the column abutting the pane reads as a border; the rest is void.
        let panes = [pane(2, 0, 3, 1, &["xyz"])];
        assert_eq!(composite_window(5, 1, &panes), " │xyz\n");
    }

    #[test]
    fn a_zero_width_pane_cannot_stall_the_walk() {
        let panes = [pane(0, 0, 0, 1, &[""]), pane(1, 0, 2, 1, &["ok"])];
        assert_eq!(composite_window(3, 1, &panes), "│ok\n");
    }

    #[test]
    fn a_styled_fill_running_to_the_window_edge_survives_the_trim() {
        // The rightmost pane ends in a background fill (a status bar). Blanket
        // trimming trailing spaces would strip the coloured cells and leave the
        // escape behind, shortening the fill.
        let filled = format!("ab{}  ", "\x1b[44m");
        let panes = [pane(0, 0, 2, 1, &["xy"]), pane(3, 0, 4, 1, &[&filled])];
        let out = composite_window(7, 1, &panes);
        assert_eq!(out, format!("xy│{filled}\n"), "styled fill was trimmed");
    }

    #[test]
    fn reset_prefixed_padding_is_still_trimmed() {
        // What `capture_rows_padded` appends: an explicit reset, then spaces.
        // Both go, since nothing is colouring them.
        let padded = format!("ab{}  ", SGR_RESET);
        let panes = [pane(0, 0, 2, 1, &["xy"]), pane(3, 0, 4, 1, &[&padded])];
        assert_eq!(composite_window(7, 1, &panes), "xy│ab\n");
    }

    #[test]
    fn trim_padding_handles_each_tail_shape() {
        assert_eq!(trim_padding("abc"), "abc", "no trailing spaces: untouched");
        assert_eq!(trim_padding("abc   "), "abc", "bare spaces: trimmed");
        assert_eq!(trim_padding("     "), "", "blank row: trims to nothing");
        assert_eq!(
            trim_padding("\x1b[31mred\x1b[0m   "),
            "\x1b[31mred",
            "reset-prefixed padding: reset and spaces both dropped"
        );
        assert_eq!(
            trim_padding("\x1b[44m   "),
            "\x1b[44m   ",
            "spaces under a live SGR: preserved"
        );
    }

    #[test]
    fn no_panes_renders_a_blank_grid() {
        // Nothing to separate, so nothing to draw: borders only appear where
        // they divide real panes.
        assert_eq!(composite_window(3, 2, &[]), "\n\n");
    }
}
