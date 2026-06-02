//! Pure helpers for the composer's `@` file-mention picker.
//!
//! Kept side-effect-free so the trigger detection, fuzzy ranking, and
//! text replacement are unit-testable without a ratatui surface or a
//! live daemon. The async fetch and all state mutation live in
//! `super::mod`; the picker open/close lifecycle lives in `state.rs`.

use ratatui_textarea::{CursorMove, TextArea};

/// Max rows the picker shows, matching the web composer's fuzzy cap.
pub const PICKER_LIMIT: usize = 30;

/// An active `@`-mention token under the composer cursor. All columns
/// are CHAR indices into the anchor row (matching `TextArea::cursor()`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mention {
    /// Logical line the token lives on.
    pub row: usize,
    /// Char index of the `@`.
    pub start_col: usize,
    /// Char index one past the end of the token (first whitespace at or
    /// after the cursor, or end of line). The full token to replace is
    /// `[start_col, end_col)`.
    pub end_col: usize,
    /// The text typed between the `@` and the token end, used as the
    /// fuzzy query.
    pub query: String,
}

/// Detect an active `@`-mention at `cursor` within `lines`.
///
/// Returns `Some` only when the cursor sits inside a contiguous,
/// whitespace-free run that starts with `@`, and that `@` is itself at
/// the start of the line or preceded by whitespace. The leading-boundary
/// rule keeps `user@host` style text from spuriously triggering the
/// picker, matching the intent of the web composer's `@` trigger.
pub fn active_mention(lines: &[String], cursor: (usize, usize)) -> Option<Mention> {
    let (row, col) = cursor;
    let line: Vec<char> = lines.get(row)?.chars().collect();
    if col > line.len() {
        return None;
    }

    // Scan left from the cursor for the `@`, aborting on whitespace.
    let mut start_col = None;
    for i in (0..col).rev() {
        let c = line[i];
        if c == '@' {
            start_col = Some(i);
            break;
        }
        if c.is_whitespace() {
            return None;
        }
    }
    let start_col = start_col?;

    // The `@` must start the line or follow whitespace.
    if start_col > 0 && !line[start_col - 1].is_whitespace() {
        return None;
    }

    // Scan right from the cursor for the token end (whitespace or EOL).
    let mut end_col = col;
    while end_col < line.len() && !line[end_col].is_whitespace() {
        end_col += 1;
    }

    let query: String = line[start_col + 1..end_col].iter().collect();
    Some(Mention {
        row,
        start_col,
        end_col,
        query,
    })
}

/// Lightweight fuzzy filter mirroring the web composer's ranking
/// (`web/src/components/cockpit/useFilesIndex.ts`): prefix matches beat
/// substring matches, ties break on shorter path. Case-insensitive. An
/// empty query returns the head of the list. Caps the result at `cap`.
pub fn fuzzy_filter<'a>(files: &'a [String], query: &str, cap: usize) -> Vec<&'a str> {
    let q = query.to_lowercase();
    if q.is_empty() {
        return files.iter().take(cap).map(String::as_str).collect();
    }
    let mut scored: Vec<(u8, usize, &str)> = files
        .iter()
        .filter_map(|f| {
            let lower = f.to_lowercase();
            let score = if lower.starts_with(&q) {
                0
            } else if lower.contains(&q) {
                1
            } else {
                return None;
            };
            Some((score, f.chars().count(), f.as_str()))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(b.2)));
    scored.into_iter().take(cap).map(|(_, _, f)| f).collect()
}

/// The text inserted into the composer for a chosen `path`. Mirrors the
/// web composer, which serializes a file mention through assistant-ui's
/// default directive formatter as `:file[<path>]` and sends that string
/// verbatim to the daemon, so both surfaces hand the agent identical
/// prompt text. A trailing space is appended unless the next character
/// is already whitespace, so the user can keep typing.
pub fn mention_replacement(path: &str, next_char: Option<char>) -> String {
    let needs_space = !matches!(next_char, Some(c) if c.is_whitespace());
    if needs_space {
        format!(":file[{path}] ")
    } else {
        format!(":file[{path}]")
    }
}

/// Replace the `@`-token described by `mention` with the directive form
/// of `path` in `textarea`, leaving the cursor just after the inserted
/// text. Char-index based throughout so multi-byte paths and queries are
/// handled correctly.
pub fn apply_selection(textarea: &mut TextArea<'static>, mention: &Mention, path: &str) {
    let next_char = textarea
        .lines()
        .get(mention.row)
        .and_then(|l| l.chars().nth(mention.end_col));
    let replacement = mention_replacement(path, next_char);

    // Position at the token end, delete the whole `@…` run, then insert.
    textarea.move_cursor(CursorMove::Jump(mention.row as u16, mention.end_col as u16));
    for _ in 0..(mention.end_col - mention.start_col) {
        textarea.delete_char();
    }
    textarea.insert_str(replacement);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn active_mention_basic_token() {
        // "see @src" with cursor at end (col 8).
        let l = lines(&["see @src"]);
        let m = active_mention(&l, (0, 8)).expect("mention");
        assert_eq!(m.start_col, 4);
        assert_eq!(m.end_col, 8);
        assert_eq!(m.query, "src");
    }

    #[test]
    fn active_mention_at_line_start() {
        let l = lines(&["@foo"]);
        let m = active_mention(&l, (0, 4)).expect("mention");
        assert_eq!(m.start_col, 0);
        assert_eq!(m.query, "foo");
    }

    #[test]
    fn active_mention_cursor_mid_token_covers_full_range() {
        // "@foobar" with cursor after "foo" (col 4). The token end must
        // still extend to the end of the run so the whole token gets
        // replaced, not just the prefix before the cursor.
        let l = lines(&["@foobar"]);
        let m = active_mention(&l, (0, 4)).expect("mention");
        assert_eq!(m.start_col, 0);
        assert_eq!(m.end_col, 7);
        assert_eq!(m.query, "foobar");
    }

    #[test]
    fn active_mention_aborts_on_whitespace_between_at_and_cursor() {
        // Cursor after the space: "@foo |" -> no contiguous token.
        let l = lines(&["@foo bar"]);
        assert_eq!(active_mention(&l, (0, 8)), None);
    }

    #[test]
    fn active_mention_requires_leading_boundary() {
        // `user@host` must not trigger: the `@` follows a non-space.
        let l = lines(&["user@host"]);
        assert_eq!(active_mention(&l, (0, 9)), None);
    }

    #[test]
    fn active_mention_none_without_at() {
        let l = lines(&["plain text"]);
        assert_eq!(active_mention(&l, (0, 5)), None);
    }

    #[test]
    fn active_mention_second_line() {
        let l = lines(&["first", "go @lib/x"]);
        let m = active_mention(&l, (1, 9)).expect("mention");
        assert_eq!(m.row, 1);
        assert_eq!(m.start_col, 3);
        assert_eq!(m.query, "lib/x");
    }

    #[test]
    fn active_mention_handles_multibyte_prefix() {
        // CJK chars before the token must not throw off char indexing.
        let l = lines(&["日本 @src/main.rs"]);
        let m = active_mention(&l, (0, 15)).expect("mention");
        assert_eq!(m.start_col, 3);
        assert_eq!(m.query, "src/main.rs");
    }

    #[test]
    fn fuzzy_filter_prefix_beats_substring() {
        let files = lines(&["zsrc/lib.rs", "src/main.rs"]);
        let out = fuzzy_filter(&files, "src", 30);
        assert_eq!(out, vec!["src/main.rs", "zsrc/lib.rs"]);
    }

    #[test]
    fn fuzzy_filter_narrows_on_longer_query() {
        let files = lines(&["src/main.rs", "src/lib.rs", "docs/readme.md"]);
        let out = fuzzy_filter(&files, "src/l", 30);
        assert_eq!(out, vec!["src/lib.rs"]);
    }

    #[test]
    fn fuzzy_filter_ties_break_on_shorter_path() {
        let files = lines(&["aa/longer.rs", "aa.rs"]);
        let out = fuzzy_filter(&files, "aa", 30);
        assert_eq!(out, vec!["aa.rs", "aa/longer.rs"]);
    }

    #[test]
    fn fuzzy_filter_empty_query_returns_head() {
        let files = lines(&["a", "b", "c"]);
        let out = fuzzy_filter(&files, "", 2);
        assert_eq!(out, vec!["a", "b"]);
    }

    #[test]
    fn fuzzy_filter_is_case_insensitive() {
        let files = lines(&["README.md"]);
        assert_eq!(fuzzy_filter(&files, "readme", 30), vec!["README.md"]);
    }

    #[test]
    fn mention_replacement_adds_trailing_space() {
        assert_eq!(mention_replacement("src/x.rs", None), ":file[src/x.rs] ");
    }

    #[test]
    fn mention_replacement_skips_space_before_whitespace() {
        assert_eq!(
            mention_replacement("src/x.rs", Some(' ')),
            ":file[src/x.rs]"
        );
    }

    #[test]
    fn apply_selection_replaces_full_token() {
        let mut ta = TextArea::from(["see @src here"]);
        // Cursor anywhere; we drive replacement off the Mention range.
        let m = active_mention(ta.lines(), (0, 8)).expect("mention");
        apply_selection(&mut ta, &m, "src/main.rs");
        // The "see " prefix and " here" suffix are untouched; only the
        // `@src` token becomes the directive. No trailing space is added
        // because the next char is already whitespace.
        assert_eq!(ta.lines(), ["see :file[src/main.rs] here"]);
    }

    #[test]
    fn apply_selection_at_end_of_line_appends_space() {
        let mut ta = TextArea::from(["open @ma"]);
        let m = active_mention(ta.lines(), (0, 8)).expect("mention");
        apply_selection(&mut ta, &m, "Makefile");
        assert_eq!(ta.lines(), ["open :file[Makefile] "]);
    }

    #[test]
    fn apply_selection_handles_multibyte_path() {
        let mut ta = TextArea::from(["ref @x"]);
        let m = active_mention(ta.lines(), (0, 6)).expect("mention");
        apply_selection(&mut ta, &m, "ドキュメント/a.md");
        assert_eq!(ta.lines(), ["ref :file[ドキュメント/a.md] "]);
    }
}
