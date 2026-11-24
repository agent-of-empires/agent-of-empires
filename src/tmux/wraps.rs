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

/// Most continuation rows one capture re-reads.
const MAX_RELINKED_ROWS: usize = 256;

/// `body` rows after `wraps` rows that ended inside a link. `capture-pane -e`
/// closes a link at every row's end and never reopens it on the continuation
/// row, though tmux's cells there still carry it.
fn unlinked_continuations(rows: &[&[u8]], wraps: &[u32]) -> Vec<usize> {
    let mut out = Vec::new();
    let mut continues = false;
    for (i, row) in rows.iter().enumerate().take(rows.len().saturating_sub(1)) {
        let ends_linked = row.ends_with(b"\x1b]8;;\x1b\\") || row.ends_with(b"\x1b]8;;\x07");
        continues = wraps.binary_search(&(i as u32)).is_ok() && (continues || ends_linked);
        if continues {
            out.push(i + 1);
        }
    }
    out.truncate(MAX_RELINKED_ROWS);
    out
}

/// Re-read the continuation rows of soft-wrapped links in `body` (a
/// `capture-pane -e -N` of `target` whose last `pane_height` rows are the
/// visible screen) one row per capture, which starts each row's escape state
/// afresh, so the link is reopened there. A row whose text changed since
/// `body` was taken is left as it was.
pub(crate) fn relink_continuations(
    target: &str,
    body: &mut Vec<u8>,
    wraps: &[u32],
    pane_height: u16,
    deadline: &crate::tmux::TmuxCommandDeadline,
) {
    let text = body.strip_suffix(b"\n").unwrap_or(body);
    let rows: Vec<&[u8]> = text.split(|&b| b == b'\n').collect();
    let wanted = unlinked_continuations(&rows, wraps);
    let Some(history) = rows.len().checked_sub(usize::from(pane_height)) else {
        return;
    };
    if wanted.is_empty() {
        return;
    }
    let lines: Vec<String> = wanted
        .iter()
        .map(|&i| (i as i64 - history as i64).to_string())
        .collect();
    let mut command = crate::tmux::tmux_command();
    for (n, line) in lines.iter().enumerate() {
        if n > 0 {
            command.arg(";");
        }
        command.args([
            "capture-pane",
            "-t",
            target,
            "-p",
            "-e",
            "-N",
            "-S",
            line,
            "-E",
            line,
        ]);
    }
    let Ok(out) = deadline.run(&mut command) else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let fresh = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
    let fresh: Vec<&[u8]> = fresh.split(|&b| b == b'\n').collect();
    if fresh.len() != wanted.len() {
        return;
    }
    *body = splice_rows(&rows, &wanted, &fresh, body.ends_with(b"\n"));
}

/// `rows` with each `at` row replaced by its `fresh` twin when the two show
/// the same text. A row capture opens from default attributes, so a reset
/// first drops whatever the previous row left set.
fn splice_rows(rows: &[&[u8]], at: &[usize], fresh: &[&[u8]], terminated: bool) -> Vec<u8> {
    let plain = |row: &[u8]| crate::tmux::utils::strip_ansi(&String::from_utf8_lossy(row));
    let mut out = Vec::new();
    for (i, row) in rows.iter().enumerate() {
        if i > 0 {
            out.push(b'\n');
        }
        let twin = at
            .binary_search(&i)
            .ok()
            .map(|n| fresh[n])
            .filter(|twin| plain(twin).trim_end() == plain(row).trim_end());
        match twin {
            Some(twin) => {
                out.extend_from_slice(b"\x1b[0m");
                out.extend_from_slice(twin);
            }
            None => out.extend_from_slice(row),
        }
    }
    if terminated {
        out.push(b'\n');
    }
    out
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
    fn rereads_only_rows_that_continue_a_wrapped_link() {
        let open = "\x1b]8;;https://e.co/a\x1b\\";
        let close = "\x1b]8;;\x1b\\";
        let linked_end = format!("{open}https://e{close}");
        let cases: &[(&str, Vec<String>, &[u32], &[usize])] = &[
            (
                "a link over three rows",
                vec![
                    linked_end.clone(),
                    "xample.co".into(),
                    "/a".into(),
                    "$".into(),
                ],
                &[0, 1],
                &[1, 2],
            ),
            (
                "a link closed before the wrap",
                vec![format!("{open}ab{close} cd"), "ef".into()],
                &[0],
                &[],
            ),
            (
                "a linked row that does not wrap",
                vec![linked_end.clone(), "next".into()],
                &[],
                &[],
            ),
            (
                "plain wrapped rows",
                vec!["abc".into(), "def".into(), "gh".into()],
                &[0, 1],
                &[],
            ),
        ];
        for (name, rows, wraps, want) in cases {
            let rows: Vec<&[u8]> = rows.iter().map(|r| r.as_bytes()).collect();
            assert_eq!(unlinked_continuations(&rows, wraps), *want, "{name}");
        }
    }

    #[test]
    fn splices_a_reread_row_only_when_its_text_still_matches() {
        let rows: &[&[u8]] = &[b"\x1b[1mab", b"cd", b"ef"];
        let fresh: &[&[u8]] = &[b"\x1b]8;;u\x1b\\cd\x1b]8;;\x1b\\", b"zz"];
        assert_eq!(
            splice_rows(rows, &[1, 2], fresh, true),
            b"\x1b[1mab\n\x1b[0m\x1b]8;;u\x1b\\cd\x1b]8;;\x1b\\\nef\n".to_vec()
        );
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
