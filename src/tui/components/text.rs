//! Small shared text helpers for TUI rendering.

/// One rendered row resolved into display columns.
///
/// Measured from the line's own graphemes. Reading a scratch buffer back
/// instead looks tempting but is wrong: `Buffer::set_line` resets the
/// continuation cell of a wide grapheme, and `Cell::symbol()` answers `" "`
/// for a reset cell, so every wide grapheme would gain a phantom space and no
/// label containing one could ever match.
pub(crate) struct LineColumns {
    /// The row's visible text, concatenated left to right.
    pub(crate) text: String,
    /// Column each byte of `text` belongs to, plus a trailing entry for the
    /// end of the row. A continuation cell contributes no byte, which is what
    /// makes an exclusive end offset resolve past both halves of a wide
    /// grapheme.
    column_of: Vec<u16>,
    /// Whether source text was dropped because it did not fit `width`. A run
    /// ending at the retained boundary may be a prefix of something longer, so
    /// anything inferred from the text alone has to refuse it.
    clipped: bool,
}

impl LineColumns {
    /// Column holding the grapheme that starts at `byte`. `byte` must be a
    /// char boundary of `text` or its length.
    pub(crate) fn column_at(&self, byte: usize) -> u16 {
        self.column_of
            .get(byte)
            .copied()
            .unwrap_or_else(|| self.column_of.last().copied().unwrap_or(0))
    }

    /// Whether the row was cut short of `width`, so its last run may be a
    /// prefix rather than the whole of what the pane holds.
    pub(crate) fn is_clipped(&self) -> bool {
        self.clipped
    }

    /// The text painted between `from` and `to_excl` display columns.
    pub(crate) fn slice(&self, from: u16, to_excl: u16) -> String {
        let mut out = String::new();
        for (offset, ch) in self.text.char_indices() {
            let col = self.column_at(offset);
            if col >= from && col < to_excl {
                out.push(ch);
            }
        }
        out
    }
}

/// Resolve `line` into display columns at `width`.
pub(crate) fn line_columns(line: &ratatui::text::Line, width: u16) -> LineColumns {
    use ratatui::buffer::CellWidth;
    use unicode_segmentation::UnicodeSegmentation;

    let mut text = String::with_capacity(width as usize);
    let mut column_of = Vec::with_capacity(width as usize);
    let mut col = 0u16;
    let mut clipped = false;
    for span in &line.spans {
        for grapheme in span.content.graphemes(true) {
            // Same filtering and the same width rule `Buffer::set_stringn` uses.
            // `CellWidth` is not `UnicodeWidthStr`: it adds a cell for halfwidth
            // katakana dakuten/handakuten, and a mapper that disagreed would
            // shift every underline, OSC 8 span and selection on the row.
            if grapheme.contains(char::is_control) {
                continue;
            }
            let cells = grapheme.cell_width();
            if cells == 0 {
                continue;
            }
            // A grapheme the renderer cannot fit is not painted, so it must not
            // be matchable either.
            if col + cells > width {
                clipped = true;
                break;
            }
            column_of.resize(column_of.len() + grapheme.len(), col);
            text.push_str(grapheme);
            col += cells;
        }
        if clipped {
            break;
        }
    }
    column_of.push(col);
    LineColumns {
        text,
        column_of,
        clipped,
    }
}

/// Truncate `text` to `max_width` display cells, appending `…` if
/// anything was dropped. Width-aware (wide glyphs count their real cell
/// width), so a truncated string never paints past its budget. Returns
/// "" when `max_width` is 0 (the text gets sacrificed entirely so
/// whatever fixed content it competes with wins).
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if rendered_width(text) <= max_width {
        return text.to_string();
    }
    // Reserve one cell for the ellipsis.
    let mut out = prefix_within_width(text, max_width.saturating_sub(1)).to_string();
    out.push('\u{2026}');
    out
}

/// Cells `text` occupies when the renderer paints it: the per-grapheme
/// `CellWidth` metric that `Span::styled_graphemes` and `Buffer::set_stringn`
/// apply, summed over clusters.
///
/// Not `UnicodeWidthStr::width`, which resolves halfwidth katakana
/// dakuten/handakuten (U+FF9E, U+FF9F) to zero cells where the renderer
/// spends one on each, so a string-width budget admits twice the text that
/// fits for that script.
pub fn rendered_width(text: &str) -> usize {
    use ratatui::buffer::CellWidth;
    use unicode_segmentation::UnicodeSegmentation;
    text.graphemes(true).map(|g| g.cell_width() as usize).sum()
}

/// The longest prefix of `text` that fits in `max_width` display cells, with
/// no ellipsis.
///
/// Steps by grapheme cluster, because a `char` is not a display unit: cutting
/// mid-cluster leaves a dangling combining mark ("क्" out of "क्ष") or strips a
/// VS16 so the base glyph flips from emoji to text presentation. Cells come
/// from [`rendered_width`], so a cluster whose scalars do not sum to what it
/// paints ("\u{26a0}\u{fe0f}" is 2 cells where its chars sum to 1,
/// "\u{1f91d}\u{1f3fd}" is 2 where they sum to 4) neither over- nor
/// under-fills the budget.
pub fn prefix_within_width(text: &str, max_width: usize) -> &str {
    use ratatui::buffer::CellWidth;
    use unicode_segmentation::UnicodeSegmentation;
    let mut cells = 0usize;
    let mut end = 0;
    for (start, g) in text.grapheme_indices(true) {
        cells += g.cell_width() as usize;
        if cells > max_width {
            break;
        }
        end = start + g.len();
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::{line_columns, prefix_within_width, rendered_width, truncate_to_width};

    /// A wide grapheme occupies two columns and its continuation cell is reset
    /// by the renderer. Reading that back out of a buffer yields a phantom
    /// space, which is what used to stop a CJK label from ever matching.
    #[test]
    fn wide_graphemes_span_two_columns_without_a_phantom_space() {
        use ratatui::text::Line;
        let columns = line_columns(&Line::from("日本語 x"), 40);
        assert_eq!(columns.text, "日本語 x", "no space injected between cells");
        assert_eq!(columns.column_at(0), 0);
        assert_eq!(columns.column_at("日".len()), 2);
        assert_eq!(columns.column_at("日本".len()), 4);
        assert_eq!(columns.column_at("日本語 ".len()), 7);
        assert_eq!(columns.slice(0, 6), "日本語");
    }

    /// A grapheme that does not fit the pane is not painted, so it must not be
    /// matchable either.
    #[test]
    fn a_grapheme_past_the_pane_edge_is_not_included() {
        use ratatui::text::Line;
        assert_eq!(line_columns(&Line::from("ab日"), 3).text, "ab");
        assert_eq!(line_columns(&Line::from("ab日"), 4).text, "ab日");
    }

    #[test]
    fn slice_returns_the_columns_asked_for() {
        use ratatui::text::Line;
        let columns = line_columns(&Line::from("hello world"), 40);
        assert_eq!(columns.slice(6, 11), "world");
        assert_eq!(columns.slice(0, 5), "hello");
    }

    #[test]
    fn prefix_within_width_measures_clusters_as_a_string() {
        // No ellipsis, and a prefix that fits comes back whole.
        assert_eq!(prefix_within_width("abc", 4), "abc");
        assert_eq!(prefix_within_width("abcdef", 4), "abcd");
        assert_eq!(prefix_within_width("abc", 0), "");
        // Per-scalar widths sum to 4 but the string is 5 cells: drop `a`.
        assert_eq!(
            prefix_within_width("\u{2665}\u{fe0f}界a", 4),
            "\u{2665}\u{fe0f}界"
        );
        // Per-scalar widths sum to 5 but the skin-tone cluster is 2 cells:
        // keep `m`.
        assert_eq!(
            prefix_within_width("\u{1f91d}\u{1f3fd}m", 4),
            "\u{1f91d}\u{1f3fd}m"
        );
    }

    /// The passthrough guard has to use the renderer's metric too: halfwidth
    /// katakana dakuten scores zero in `UnicodeWidthStr`, so a string-width
    /// guard hands back twice the budget untouched.
    #[test]
    fn truncate_to_width_guard_uses_the_rendered_metric() {
        let halfwidth = "\u{ff8a}\u{ff9f}".repeat(20);
        assert_eq!(rendered_width(&halfwidth), 40);
        let out = truncate_to_width(&halfwidth, 24);
        assert!(rendered_width(&out) <= 24, "{out:?}");
        assert!(out.ends_with('\u{2026}'));
    }

    #[test]
    fn truncate_to_width_passthrough_when_fits() {
        assert_eq!(truncate_to_width("hello", 10), "hello");
        assert_eq!(truncate_to_width("hello", 5), "hello");
    }

    #[test]
    fn truncate_to_width_appends_ellipsis_when_overflow() {
        assert_eq!(truncate_to_width("abcdefg", 5), "abcd\u{2026}");
    }

    #[test]
    fn truncate_to_width_zero_returns_empty() {
        assert_eq!(truncate_to_width("abc", 0), "");
    }

    #[test]
    fn truncate_to_width_counts_wide_glyphs() {
        // Each CJK glyph is two cells wide; a 5-cell budget fits two
        // glyphs (4 cells) plus the ellipsis.
        assert_eq!(truncate_to_width("日本語です", 5), "日本\u{2026}");
    }

    #[test]
    fn truncate_to_width_never_exceeds_the_budget() {
        use unicode_width::UnicodeWidthStr;
        // Emoji-presentation sequences (base char plus VS16) measure 2 cells as
        // a cluster while their chars sum to 1, so a per-char budget admitted
        // one glyph too many and returned an over-wide string.
        for text in [
            "\u{26a0}\u{fe0f} CI failing on 5 checks",
            "\u{2764}\u{fe0f}\u{2764}\u{fe0f} very long status text here",
            "\u{2139}\u{fe0f}\u{2139}\u{fe0f}\u{2139}\u{fe0f} info info info info",
            "\u{274c} changes requested on this pull request",
            "検査失敗検査失敗検査失敗中",
            "plain ascii status text that is comfortably too long",
        ] {
            for budget in 1..=24 {
                let out = truncate_to_width(text, budget);
                assert!(
                    UnicodeWidthStr::width(out.as_str()) <= budget,
                    "{text:?} at budget {budget} returned {} cells",
                    UnicodeWidthStr::width(out.as_str())
                );
            }
        }
    }

    #[test]
    fn truncate_to_width_never_splits_a_grapheme_cluster() {
        use unicode_segmentation::UnicodeSegmentation;
        // Cutting per char can land inside a cluster: "क्ष" would lose its
        // final consonant and leave a dangling virama, and "\u{26a0}\u{fe0f}"
        // would lose the VS16 and flip to text presentation.
        assert_eq!(truncate_to_width("क्षx", 2), "\u{2026}");
        assert_eq!(truncate_to_width("\u{26a0}\u{fe0f}abc", 2), "\u{2026}");
        for text in [
            "क्षक्षक्ष trailing text",
            "\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} family status",
            "\u{26a0}\u{fe0f}\u{2764}\u{fe0f} mixed presentation",
        ] {
            for budget in 1..=24 {
                let out = truncate_to_width(text, budget);
                let body = out.strip_suffix('\u{2026}').unwrap_or(&out);
                // The kept part must be a whole number of clusters, so it has
                // to equal one of the cluster-aligned prefixes of the input.
                let aligned = std::iter::once(String::new())
                    .chain(text.graphemes(true).scan(String::new(), |acc, g| {
                        acc.push_str(g);
                        Some(acc.clone())
                    }))
                    .any(|prefix| prefix == body);
                assert!(
                    aligned,
                    "{text:?} at budget {budget} cut mid-cluster: {body:?}"
                );
            }
        }
    }
}
