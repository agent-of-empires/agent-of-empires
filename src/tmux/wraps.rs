//! Which rows of a `capture-pane` body soft-wrap onto the next row.
//!
//! tmux flags each wrapped grid line but `capture-pane` prints no flag; `-J`
//! joins exactly the flagged rows instead. Comparing a row capture with a
//! joined capture of the same instant recovers the flags.

use unicode_width::UnicodeWidthChar;

/// Lines of a capture, each terminated by `\n` (the last one included).
fn terminated_lines(text: &str) -> impl Iterator<Item = &str> {
    text.strip_suffix('\n').unwrap_or(text).split('\n')
}

/// Indices of `body` rows (`capture-pane -e`, escapes allowed) whose line
/// continues on the next row, given the joined plain capture `joined`
/// (`capture-pane -J`) of the same window. Empty when the two disagree,
/// which is how a capture that raced pane output shows up.
pub(crate) fn soft_wraps(body: &str, joined: &str) -> Vec<u32> {
    if body.is_empty() || joined.is_empty() {
        return Vec::new();
    }
    let rows: Vec<String> = terminated_lines(body)
        .map(crate::tmux::utils::strip_ansi)
        .collect();
    let mut wrapped = Vec::new();
    let mut i = 0usize;
    for line in terminated_lines(joined) {
        let mut rest = line;
        loop {
            let Some(row) = rows.get(i) else {
                return Vec::new();
            };
            // `-N` and `-J` may disagree on padding at a row's end.
            let Some(after) = rest
                .strip_prefix(row.as_str())
                .or_else(|| rest.strip_prefix(row.trim_end()))
            else {
                return Vec::new();
            };
            rest = after;
            i += 1;
            if rest.trim_end().is_empty() {
                break;
            }
            wrapped.push((i - 1) as u32);
        }
    }
    if i != rows.len() {
        return Vec::new();
    }
    wrapped
}

/// A line no pane prints, to separate a row capture from its joined twin in
/// one chained tmux invocation's output.
pub(crate) fn sentinel() -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("@@aoe-joined-{:x}-{nanos:x}-{n:x}@@", std::process::id())
}

/// `raw` split around its `sentinel` line: the text before it (with its
/// terminator) and the text after it. `None` when the line is missing.
pub(crate) fn split_at_sentinel<'a>(raw: &'a [u8], sentinel: &str) -> Option<(&'a [u8], &'a [u8])> {
    let line = format!("{sentinel}\n");
    let line = line.as_bytes();
    if raw.starts_with(line) {
        return Some((&raw[..0], &raw[line.len()..]));
    }
    let at = raw
        .windows(line.len() + 1)
        .position(|w| w[0] == b'\n' && &w[1..] == line)?;
    Some((&raw[..=at], &raw[at + 1 + line.len()..]))
}

/// Columns vt100 gives `text`, which must hold no escapes.
pub(crate) fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(1).min(2)).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovers_wraps_from_a_joined_capture() {
        let cases: &[(&str, &str, &str, &[u32])] = &[
            ("no wraps", "ab\ncd\n", "ab\ncd\n", &[]),
            (
                "one url over three rows",
                "$ echo\nhttps://e\nxample.co\nm/a\n$\n",
                "$ echo\nhttps://example.com/a\n$\n",
                &[1, 2],
            ),
            (
                "styled rows",
                "\x1b[31mhttps://e\x1b[0m\n\x1b[31mxample\x1b[0m\n",
                "https://example\n",
                &[0],
            ),
            (
                "padding differs at the wrap",
                "abcd \nef\n",
                "abcdef\n",
                &[0],
            ),
            (
                "trailing blank rows",
                "abcd\nef\n\n\n",
                "abcdef\n\n\n",
                &[0],
            ),
            (
                "the joined capture raced output",
                "abcd\nef\n",
                "zzzz\n",
                &[],
            ),
            ("row count disagrees", "ab\ncd\nef\n", "ab\ncd\n", &[]),
            ("nothing captured", "", "", &[]),
        ];
        for (name, body, joined, want) in cases {
            assert_eq!(soft_wraps(body, joined), *want, "{name}");
        }
    }

    #[test]
    fn splits_a_chained_capture_at_its_sentinel() {
        let mark = sentinel();
        assert_ne!(mark, sentinel());
        let raw = format!("a\nb\n{mark}\nab\n");
        assert_eq!(
            split_at_sentinel(raw.as_bytes(), &mark),
            Some((&b"a\nb\n"[..], &b"ab\n"[..]))
        );
        let empty = format!("{mark}\n");
        assert_eq!(
            split_at_sentinel(empty.as_bytes(), &mark),
            Some((&b""[..], &b""[..]))
        );
        assert_eq!(split_at_sentinel(b"a\nb\n", &mark), None);
    }
}
