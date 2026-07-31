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
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    // Measure the accumulated prefix rather than summing per-char widths. The
    // two disagree: `UnicodeWidthStr::width` resolves grapheme clusters, so an
    // emoji-presentation sequence like "\u{26a0}\u{fe0f}" is 2 cells while its
    // chars sum to 1. Summing therefore over-admits and returns a string wider
    // than the budget, breaking the promise above (and underflowing any caller
    // that subtracts the result from a remaining budget).
    for c in text.chars() {
        out.push(c);
        if UnicodeWidthStr::width(out.as_str()) > budget {
            out.pop();
            break;
        }
    }
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::truncate_to_width;

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
}
