//! Splice a tmux window's panes into one screen-shaped snapshot with borders.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PaneGeom {
    pub left: u16,
    pub top: u16,
    pub width: u16,
    pub height: u16,
}

impl PaneGeom {
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

    /// Zoomed panes overlap their neighbours; compositing requires a tiling.
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

pub(crate) struct CapturedPane {
    pub geom: PaneGeom,
    pub rows: Vec<String>,
}

/// Cached across frames: watched panes refresh lazily, pane 0 every frame.
pub(crate) struct WindowLayout {
    pub window_width: u16,
    pub window_height: u16,
    pub panes: Vec<CapturedPane>,
}

impl WindowLayout {
    pub(crate) fn composite(&self) -> String {
        composite_window(self.window_width, self.window_height, &self.panes)
    }

    /// The top-left pane can have a non-zero origin when chrome reserves space.
    pub(crate) fn first_pane(&self) -> Option<PaneGeom> {
        self.panes.first().map(|p| p.geom)
    }

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

/// Drop trailing padding. Spaces after a live SGR are a coloured fill, not
/// padding; padding is always introduced by [`SGR_RESET`].
fn trim_padding(line: &str) -> &str {
    let trimmed = line.trim_end_matches(' ');
    if trimmed.len() == line.len() {
        return line;
    }
    if let Some(rest) = trimmed.strip_suffix(SGR_RESET) {
        return rest;
    }
    if !trimmed.contains('\x1b') {
        return trimmed;
    }
    line
}

const SGR_RESET: &str = "\x1b[0m";

const BORDER_VERTICAL: char = '│';
const BORDER_HORIZONTAL: char = '─';
const BORDER_CROSS: char = '┼';

/// Lay `panes` onto the window grid. Every row is terminated by `\n` like
/// `capture-pane`, so a blank last row still counts. Pane rows are emitted
/// whole (never sliced mid-SGR); unattributable columns degrade to border fill.
pub(crate) fn composite_window(
    window_width: u16,
    window_height: u16,
    panes: &[CapturedPane],
) -> String {
    let mut out = String::new();
    for row in 0..window_height {
        // A gap is a horizontal rule next to a pane above/below, vertical next to one
        // left/right, a cross where only a diagonal touches, and blank otherwise.
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
        // Pane content may leave its SGR live; reset before drawing a border.
        let mut sgr_live = false;
        while col < window_width {
            let hit = panes
                .iter()
                .find(|p| p.geom.left == col && p.geom.covers_row(row));
            match hit {
                Some(pane) if pane.geom.width > 0 => {
                    if let Some(text) = pane.rows.get((row - pane.geom.top) as usize) {
                        line.push_str(text);
                        sgr_live = text.contains('\x1b') && !text.ends_with(SGR_RESET);
                    } else {
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
        let panes = [pane(0, 0, 4, 1, &["topp"]), pane(0, 2, 4, 1, &["botm"])];
        assert_eq!(composite_window(4, 3, &panes), "topp\n────\nbotm\n");
    }

    #[test]
    fn a_pane_shorter_than_its_neighbour_pads_rather_than_shifting() {
        let panes = [
            pane(0, 0, 3, 1, &["abc"]),
            pane(4, 0, 3, 2, &["xyz", "uvw"]),
        ];
        assert_eq!(composite_window(7, 2, &panes), "abc│xyz\n───│uvw\n");
    }

    #[test]
    fn a_row_missing_from_a_capture_pads_its_width() {
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
        assert_eq!(l.composite(), "old1│keep\nold2│same\n");
    }

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

    #[test]
    fn overlapping_rectangles_are_detected() {
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
        let empty = PaneGeom {
            left: 0,
            top: 0,
            width: 0,
            height: 8,
        };
        assert!(!empty.overlaps(&unzoomed_0));
    }

    #[test]
    fn a_rule_crossing_draws_a_junction_not_a_hole() {
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

    #[test]
    fn a_dead_corner_stays_blank() {
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
        let left = "\x1b[0m\x1b[31mred\x1b[0m";
        let right = "\x1b[0m\x1b[32mgrn\x1b[0m";
        let panes = [pane(0, 0, 3, 1, &[left]), pane(4, 0, 3, 1, &[right])];
        let out = composite_window(7, 1, &panes);
        assert_eq!(out, format!("{left}│{right}\n"));
    }

    #[test]
    fn an_unclaimed_column_degrades_to_border_fill() {
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
        let filled = format!("ab{}  ", "\x1b[44m");
        let panes = [pane(0, 0, 2, 1, &["xy"]), pane(3, 0, 4, 1, &[&filled])];
        let out = composite_window(7, 1, &panes);
        assert_eq!(out, format!("xy│{filled}\n"), "styled fill was trimmed");
    }

    #[test]
    fn reset_prefixed_padding_is_still_trimmed() {
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
        assert_eq!(composite_window(3, 2, &[]), "\n\n");
    }
}
