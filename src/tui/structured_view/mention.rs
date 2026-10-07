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
/// (`web/src/components/acp/useFilesIndex.ts`): prefix matches beat
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
    fn active_mention_cases() {
        // (lines, cursor, Some((row, start_col, end_col, query)))
        type Case<'a> = (
            &'a [&'a str],
            (usize, usize),
            Option<(usize, usize, usize, &'a str)>,
        );
        let cases: &[Case] = &[
            (&["see @src"], (0, 8), Some((0, 4, 8, "src"))),
            (&["@foo"], (0, 4), Some((0, 0, 4, "foo"))),
            // Cursor mid-token: the range still covers the whole run so the
            // entire token gets replaced.
            (&["@foobar"], (0, 4), Some((0, 0, 7, "foobar"))),
            // Whitespace between `@` and the cursor: no contiguous token.
            (&["@foo bar"], (0, 8), None),
            // `user@host` must not trigger: the `@` follows a non-space.
            (&["user@host"], (0, 9), None),
            (&["plain text"], (0, 5), None),
            (&["first", "go @lib/x"], (1, 9), Some((1, 3, 9, "lib/x"))),
            // CJK chars before the token must not throw off char indexing.
            (
                &["日本 @src/main.rs"],
                (0, 15),
                Some((0, 3, 15, "src/main.rs")),
            ),
        ];
        for (text, cursor, want) in cases {
            let got = active_mention(&lines(text), *cursor)
                .map(|m| (m.row, m.start_col, m.end_col, m.query));
            let want = want.map(|(r, s, e, q)| (r, s, e, q.to_string()));
            assert_eq!(got, want, "{text:?} at {cursor:?}");
        }
    }

    #[test]
    fn fuzzy_filter_cases() {
        let cases: &[(&[&str], &str, usize, &[&str])] = &[
            // Prefix beats substring.
            (
                &["zsrc/lib.rs", "src/main.rs"],
                "src",
                30,
                &["src/main.rs", "zsrc/lib.rs"],
            ),
            (
                &["src/main.rs", "src/lib.rs", "docs/readme.md"],
                "src/l",
                30,
                &["src/lib.rs"],
            ),
            // Ties break on the shorter path.
            (
                &["aa/longer.rs", "aa.rs"],
                "aa",
                30,
                &["aa.rs", "aa/longer.rs"],
            ),
            (&["a", "b", "c"], "", 2, &["a", "b"]),
            (&["README.md"], "readme", 30, &["README.md"]),
        ];
        for (files, query, limit, want) in cases {
            assert_eq!(
                fuzzy_filter(&lines(files), query, *limit),
                *want,
                "{query:?}"
            );
        }
    }

    #[test]
    fn apply_selection_replaces_the_token_and_spaces_only_at_line_end() {
        assert_eq!(mention_replacement("src/x.rs", None), ":file[src/x.rs] ");
        assert_eq!(
            mention_replacement("src/x.rs", Some(' ')),
            ":file[src/x.rs]"
        );
        for (line, col, pick, want) in [
            // Prefix and suffix untouched; no extra space before whitespace.
            (
                "see @src here",
                8,
                "src/main.rs",
                "see :file[src/main.rs] here",
            ),
            ("open @ma", 8, "Makefile", "open :file[Makefile] "),
            (
                "ref @x",
                6,
                "ドキュメント/a.md",
                "ref :file[ドキュメント/a.md] ",
            ),
        ] {
            let mut ta = TextArea::from([line]);
            let m = active_mention(ta.lines(), (0, col)).expect("mention");
            apply_selection(&mut ta, &m, pick);
            assert_eq!(ta.lines(), [want]);
        }
    }
}
