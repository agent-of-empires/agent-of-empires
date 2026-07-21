//! Small shared text helpers for TUI rendering.

/// Truncate `text` to `max_width` display cells, appending `…` if
/// anything was dropped. Width-aware (wide glyphs count their real cell
/// width), so a truncated string never paints past its budget. Returns
/// "" when `max_width` is 0 (the text gets sacrificed entirely so
/// whatever fixed content it competes with wins).
pub fn truncate_to_width(text: &str, max_width: usize) -> String {
    use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    // Reserve one cell for the ellipsis.
    let budget = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0;
    for c in text.chars() {
        let cw = UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > budget {
            break;
        }
        out.push(c);
        w += cw;
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
}
