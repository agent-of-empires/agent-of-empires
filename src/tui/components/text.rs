//! Small shared text helpers for TUI rendering.

/// Truncate `text` to `max_width` display cells, appending `…` if
/// anything was dropped. Width-aware (wide glyphs count their real cell
/// width), so a truncated string never paints past its budget. Returns
/// "" when `max_width` is 0 (the text gets sacrificed entirely so
/// whatever fixed content it competes with wins).
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    use unicode_width::UnicodeWidthStr;
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    // Reserve one cell for the ellipsis.
    let mut out = prefix_within_width(text, max_width.saturating_sub(1)).to_string();
    out.push('\u{2026}');
    out
}

/// The longest prefix of `text` that fits in `max_width` display cells,
/// with no ellipsis. Steps by grapheme cluster and measures the accumulated
/// prefix rather than summing per-piece widths. Both matter, for different
/// reasons.
///
/// Clusters, because a `char` is not a display unit: cutting mid-cluster
/// leaves a dangling combining mark ("क्" out of "क्ष") or strips a VS16 so
/// the base glyph flips from emoji to text presentation.
///
/// Accumulated measurement, because `UnicodeWidthStr::width` resolves those
/// clusters, so "\u{26a0}\u{fe0f}" is 2 cells while its chars sum to 1 and
/// "\u{1f91d}\u{1f3fd}" is 2 cells while its chars sum to 4. Summing
/// over-admits the first (a string wider than the budget, which underflows
/// any caller that subtracts the result from a remaining budget) and
/// under-admits the second.
pub fn prefix_within_width(text: &str, max_width: usize) -> &str {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut end = 0;
    for (start, g) in text.grapheme_indices(true) {
        if UnicodeWidthStr::width(&text[..start + g.len()]) > max_width {
            break;
        }
        end = start + g.len();
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::{prefix_within_width, truncate_to_width};

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
