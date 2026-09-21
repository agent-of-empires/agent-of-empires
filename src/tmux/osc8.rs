//! OSC 8 hyperlink extraction. `vt100` drops OSC 8, so targets are kept beside
//! the grid, paired with the visible text they wrapped.

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneLink {
    /// Styling escapes removed so it matches the rendered row cell for cell.
    pub(crate) text: String,
    pub(crate) uri: String,
}

const OSC8_OPEN: &[u8] = b"\x1b]8;";

/// Every visible row is searched for each link on every frame.
pub(crate) const MAX_PANE_LINKS: usize = 64;

const MAX_PENDING: usize = 8192;

const MAX_URI: usize = 2048;

/// Only http(s) reaches a browser; a control byte could inject escapes.
fn usable_uri(uri: &str) -> bool {
    crate::util::is_http_url(uri) && !uri.chars().any(char::is_control)
}

pub(crate) struct Osc8Scanner {
    buf: Vec<u8>,
}

enum Seq {
    Incomplete,
    Skip(usize),
    /// A complete sequence. An empty `uri` is the closing form (`ESC ] 8 ; ; ST`).
    Found {
        uri: String,
        consumed: usize,
    },
}

impl Osc8Scanner {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<PaneLink> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some(start) = find(&self.buf, OSC8_OPEN) else {
                let keep = self.buf.len().min(OSC8_OPEN.len() - 1);
                self.buf.drain(..self.buf.len() - keep);
                break;
            };
            self.buf.drain(..start);
            let (uri, consumed) = match parse_seq(&self.buf) {
                Seq::Incomplete => {
                    if self.buf.len() > MAX_PENDING {
                        self.buf.clear();
                    }
                    break;
                }
                Seq::Skip(n) => {
                    self.buf.drain(..n);
                    continue;
                }
                Seq::Found { uri, consumed } => (uri, consumed),
            };
            if uri.is_empty() {
                self.buf.drain(..consumed);
                continue;
            }
            // The link text runs to the next OSC 8: its own close or the next open.
            let Some(next) = find(&self.buf[consumed..], OSC8_OPEN) else {
                if self.buf.len() - consumed > MAX_PENDING {
                    self.buf.drain(..consumed);
                    continue;
                }
                break;
            };
            let text = crate::tmux::utils::strip_ansi(&String::from_utf8_lossy(
                &self.buf[consumed..consumed + next],
            ));
            if !text.trim().is_empty() && usable_uri(&uri) {
                out.push(PaneLink { text, uri });
            }
            self.buf.drain(..consumed + next);
        }
        out
    }

    /// Emit a link whose close never arrived, for one-shot (non-streaming) input.
    pub(crate) fn finish(&mut self) -> Option<PaneLink> {
        let start = find(&self.buf, OSC8_OPEN)?;
        self.buf.drain(..start);
        let Seq::Found { uri, consumed } = parse_seq(&self.buf) else {
            return None;
        };
        if uri.is_empty() || !usable_uri(&uri) {
            return None;
        }
        let text = crate::tmux::utils::strip_ansi(&String::from_utf8_lossy(&self.buf[consumed..]));
        self.buf.clear();
        (!text.trim().is_empty()).then_some(PaneLink { text, uri })
    }
}

pub(crate) fn extract_links(content: &[u8]) -> Vec<PaneLink> {
    let mut scanner = Osc8Scanner::new();
    let mut out = scanner.feed(content);
    out.extend(scanner.finish());
    out
}

/// Distinguishes "no hyperlink advertised" from "one did not parse".
pub(crate) fn has_hyperlink(content: &[u8]) -> bool {
    find(content, OSC8_OPEN).is_some()
}

fn parse_seq(buf: &[u8]) -> Seq {
    let body = &buf[OSC8_OPEN.len()..];
    let (payload_len, seq_len) = match terminator(body) {
        Some(Some(pair)) => pair,
        Some(None) => return Seq::Skip(OSC8_OPEN.len()),
        None => return Seq::Incomplete,
    };
    let consumed = OSC8_OPEN.len() + seq_len;
    let Some(sep) = body[..payload_len].iter().position(|&b| b == b';') else {
        return Seq::Skip(consumed);
    };
    let uri = &body[sep + 1..payload_len];
    if uri.len() > MAX_URI {
        return Seq::Skip(consumed);
    }
    match std::str::from_utf8(uri) {
        Ok(uri) => Seq::Found {
            uri: uri.to_string(),
            consumed,
        },
        Err(_) => Seq::Skip(consumed),
    }
}

/// `(payload length, sequence length)`; `None` = not arrived yet, `Some(None)` =
/// an ESC in the payload that does not open a terminator.
fn terminator(body: &[u8]) -> Option<Option<(usize, usize)>> {
    for (i, &b) in body.iter().enumerate() {
        if b == 0x07 {
            return Some(Some((i, i + 1)));
        }
        if b != 0x1b {
            continue;
        }
        // ST is `ESC \`; a tmux passthrough wrap doubles the inner ESCs.
        let mut j = i;
        while body.get(j) == Some(&0x1b) {
            j += 1;
        }
        return match body.get(j) {
            Some(b'\\') => Some(Some((i, j + 1))),
            Some(_) => Some(None),
            None => None,
        };
    }
    None
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    let mut from = 0;
    while let Some(offset) = haystack[from..].iter().position(|b| *b == needle[0]) {
        let start = from + offset;
        if haystack.get(start..start + needle.len())? == needle {
            return Some(start);
        }
        from = start + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(text: &str, uri: &str) -> PaneLink {
        PaneLink {
            text: text.to_string(),
            uri: uri.to_string(),
        }
    }

    #[test]
    fn extracts_link_whose_text_hides_the_target() {
        assert_eq!(
            extract_links(b"\x1b]8;;https://example.com\x1b\\Click Here\x1b]8;;\x1b\\"),
            vec![link("Click Here", "https://example.com")]
        );
    }

    #[test]
    fn extracts_across_forms_and_neighbours() {
        let cases: Vec<(&str, Vec<PaneLink>)> = vec![
            (
                "before \x1b]8;;https://a.com\x1b\\text\x1b]8;;\x1b\\ after",
                vec![link("text", "https://a.com")],
            ),
            (
                "\x1b]8;;https://a.com\x1b\\A\x1b]8;;\x1b\\ and \x1b]8;;https://b.com\x1b\\B\x1b]8;;\x1b\\",
                vec![link("A", "https://a.com"), link("B", "https://b.com")],
            ),
            (
                "\x1b]8;id=abc;https://a.com\x1b\\A\x1b]8;;\x1b\\",
                vec![link("A", "https://a.com")],
            ),
            (
                "\x1b]8;;https://a.com\x07A\x1b]8;;\x07",
                vec![link("A", "https://a.com")],
            ),
            (
                "\x1b]8;;https://a.com\x1b\\\x1b[32mgreen\x1b[0m\x1b]8;;\x1b\\",
                vec![link("green", "https://a.com")],
            ),
            (
                "\x1b]8;;https://a.com\x1b\\A\x1b]8;;https://b.com\x1b\\B\x1b]8;;\x1b\\",
                vec![link("A", "https://a.com"), link("B", "https://b.com")],
            ),
            ("\x1b]8;;file:///etc/passwd\x1b\\pw\x1b]8;;\x1b\\", vec![]),
            (
                "\x1b]8;;javascript:alert(1)\x1b\\click\x1b]8;;\x1b\\",
                vec![],
            ),
            ("\x1b]8;;https://a.com\x1b\\\x1b]8;;\x1b\\", vec![]),
            ("\x1b]8;;\x1b\\plain", vec![]),
            ("no links here", vec![]),
            ("\x1b]0;Window Title\x07text", vec![]),
        ];
        for (input, expected) in cases {
            assert_eq!(extract_links(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn extracts_real_claude_code_hyperlinks() {
        let cases = [
            (
                concat!(
                    "\x1b[38;5;231m\x1b[49m\u{25cf}\x1b[39m \x1b[94m",
                    "\x1b]8;id=1nl9mmd;https://github.com/agent-of-empires/agent-of-empires\x1b\\",
                    "the AoE repo\x1b[39m\x1b]8;;\x1b\\"
                ),
                link(
                    "the AoE repo",
                    "https://github.com/agent-of-empires/agent-of-empires",
                ),
            ),
            (
                concat!(
                    " \x1b[38;5;246m\x1b]8;id=zaxmda;https://code.claude.com/docs/en/security\x1b\\",
                    "Security guide\x1b[39m\x1b]8;;\x1b\\"
                ),
                link("Security guide", "https://code.claude.com/docs/en/security"),
            ),
        ];
        for (row, expected) in cases {
            assert_eq!(extract_links(row.as_bytes()), vec![expected], "{row:?}");
        }
    }

    #[test]
    fn extracts_link_split_across_chunks() {
        let raw = b"\x1b]8;;https://example.com\x1b\\Click Here\x1b]8;;\x1b\\";
        for split in 1..raw.len() {
            let mut scanner = Osc8Scanner::new();
            let mut out = scanner.feed(&raw[..split]);
            out.extend(scanner.feed(&raw[split..]));
            assert_eq!(
                out,
                vec![link("Click Here", "https://example.com")],
                "split at {split}"
            );
        }
    }

    #[test]
    fn finish_emits_a_link_whose_close_never_arrived() {
        assert_eq!(
            extract_links(b"\x1b]8;;https://example.com\x1b\\Click Here"),
            vec![link("Click Here", "https://example.com")]
        );
        assert_eq!(extract_links(b"\x1b]8;;https://example.com"), vec![]);
    }

    #[test]
    fn unbounded_link_text_is_dropped_rather_than_buffered() {
        let mut scanner = Osc8Scanner::new();
        assert!(scanner.feed(b"\x1b]8;;https://a.com\x1b\\").is_empty());
        assert!(scanner.feed(&vec![b'x'; MAX_PENDING + 1]).is_empty());
        assert!(scanner.buf.len() < OSC8_OPEN.len());
    }

    #[test]
    fn plain_output_does_not_grow_the_buffer() {
        let mut scanner = Osc8Scanner::new();
        for _ in 0..64 {
            assert!(scanner.feed(&vec![b'x'; 4096]).is_empty());
        }
        assert!(scanner.buf.len() < OSC8_OPEN.len());
    }
}
