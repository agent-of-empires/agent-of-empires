//! OSC 8 hyperlink extraction from a pane's byte stream.
//!
//! A hyperlink target cannot ride along with the text it wraps: `vt100` routes
//! OSC 8 to its unhandled-sequence hook and drops it, and a ratatui cell has no
//! attribute to hold a URI. So the targets are kept beside the grid instead,
//! paired with the visible text they wrapped, and the TUI re-finds that text on
//! the rendered row to decide what a click opens.

/// A hyperlink the pane advertised via OSC 8.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PaneLink {
    /// Visible text between the opening and closing sequence, with styling
    /// escapes removed so it matches the rendered row cell for cell.
    pub(crate) text: String,
    pub(crate) uri: String,
}

const OSC8_OPEN: &[u8] = b"\x1b]8;";

/// Ceiling on the links one pane is resolved against. Every visible row is
/// searched for each of them on every frame, so an unbounded table turns a
/// link-dense scrollback into frame time. Shared by the channel's table and the
/// capture path's, which must not drift apart.
pub(crate) const MAX_PANE_LINKS: usize = 64;

/// Cap on the bytes held while a sequence or its link text is still
/// incomplete. Bounds what a truncated or malformed sequence can accumulate.
const MAX_PENDING: usize = 8192;

/// Cap on one target's length. Anything longer is not a URL anyone typed.
const MAX_URI: usize = 2048;

/// Whether a parsed target is safe to keep. Only `http`/`https` reach a
/// browser opener, and a control byte inside one is never legitimate: it would
/// terminate a re-emitted sequence early and let pane output inject escapes
/// into aoe's own stream.
fn usable_uri(uri: &str) -> bool {
    crate::util::is_http_url(uri) && !uri.chars().any(char::is_control)
}

/// Incremental OSC 8 extractor for the raw pane stream.
///
/// The stream arrives in arbitrary read-sized chunks, so the scanner keeps the
/// bytes of any sequence (or link text) it has not resolved yet and resumes on
/// the next [`feed`](Self::feed). Modeled on `Osc52Scanner` in `vt.rs`, which
/// taps the same stream for clipboard writes.
pub(crate) struct Osc8Scanner {
    buf: Vec<u8>,
}

/// One `ESC ] 8 ;` sequence parsed off the head of a buffer.
enum Seq {
    /// The sequence has not fully arrived.
    Incomplete,
    /// `consumed` leading bytes are unusable; skip them.
    Skip(usize),
    /// A complete sequence. An empty `uri` is the closing form (`ESC ] 8 ; ; ST`).
    Found { uri: String, consumed: usize },
}

impl Osc8Scanner {
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Scan one chunk, returning every hyperlink completed by it.
    pub(crate) fn feed(&mut self, chunk: &[u8]) -> Vec<PaneLink> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        loop {
            let Some(start) = find(&self.buf, OSC8_OPEN) else {
                // Nothing pending but a possible split `ESC ] 8` prefix.
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
                // A close with no open ahead of it (the text was already
                // emitted, or the open predates this scanner).
                self.buf.drain(..consumed);
                continue;
            }
            // The link text runs to whatever OSC 8 comes next: its own close,
            // or the open of a link that followed without one.
            let Some(next) = find(&self.buf[consumed..], OSC8_OPEN) else {
                if self.buf.len() - consumed > MAX_PENDING {
                    // The close is never coming; drop the open and let the
                    // next pass discard the text it was holding.
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

    /// Emit a link whose closing sequence never arrived, using the text held so
    /// far. For the one-shot string path, where the capture simply ends after
    /// the link text; the streaming path keeps waiting instead.
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

/// Extract every hyperlink in a complete capture. One-shot form of
/// [`Osc8Scanner`], for content that arrives whole (a frame, a `capture-pane`
/// seed) rather than as a stream.
pub(crate) fn extract_links(content: &[u8]) -> Vec<PaneLink> {
    let mut scanner = Osc8Scanner::new();
    let mut out = scanner.feed(content);
    out.extend(scanner.finish());
    out
}

/// Whether `content` holds anything that opens an OSC 8 sequence. Distinguishes
/// "the pane advertised no hyperlink" from "one was advertised and did not
/// parse", which the caller logs.
pub(crate) fn has_hyperlink(content: &[u8]) -> bool {
    find(content, OSC8_OPEN).is_some()
}

/// Parse the sequence at the head of `buf`, which starts with `ESC ] 8 ;`.
fn parse_seq(buf: &[u8]) -> Seq {
    let body = &buf[OSC8_OPEN.len()..];
    let (payload_len, seq_len) = match terminator(body) {
        Some(Some(pair)) => pair,
        // A stray ESC inside the payload: not this sequence's terminator, so
        // resume scanning from it.
        Some(None) => return Seq::Skip(OSC8_OPEN.len()),
        None => return Seq::Incomplete,
    };
    let consumed = OSC8_OPEN.len() + seq_len;
    // `8 ; <params> ; <uri>`. Params carry an optional `id=`; nothing here
    // needs them.
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

/// Locate the OSC terminator in `body`, returning `(payload length, sequence
/// length)`. `None` means it has not arrived; `Some(None)` means the payload
/// holds an ESC that does not open one.
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

/// Anchor on the leading ESC before comparing. Every capture runs through this
/// on each content change and almost none hold a hyperlink, so the scan wants
/// to be a byte search rather than a sliding window compare.
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
            // Surrounding text is not part of the link.
            (
                "before \x1b]8;;https://a.com\x1b\\text\x1b]8;;\x1b\\ after",
                vec![link("text", "https://a.com")],
            ),
            // Two links on one row.
            (
                "\x1b]8;;https://a.com\x1b\\A\x1b]8;;\x1b\\ and \x1b]8;;https://b.com\x1b\\B\x1b]8;;\x1b\\",
                vec![link("A", "https://a.com"), link("B", "https://b.com")],
            ),
            // An `id=` param is ignored.
            (
                "\x1b]8;id=abc;https://a.com\x1b\\A\x1b]8;;\x1b\\",
                vec![link("A", "https://a.com")],
            ),
            // BEL terminates the sequence too.
            (
                "\x1b]8;;https://a.com\x07A\x1b]8;;\x07",
                vec![link("A", "https://a.com")],
            ),
            // Styling inside the link text is stripped, so the text matches the
            // row as rendered.
            (
                "\x1b]8;;https://a.com\x1b\\\x1b[32mgreen\x1b[0m\x1b]8;;\x1b\\",
                vec![link("green", "https://a.com")],
            ),
            // A link left open when the next one starts still resolves.
            (
                "\x1b]8;;https://a.com\x1b\\A\x1b]8;;https://b.com\x1b\\B\x1b]8;;\x1b\\",
                vec![link("A", "https://a.com"), link("B", "https://b.com")],
            ),
            // Non-http targets never reach the browser opener.
            ("\x1b]8;;file:///etc/passwd\x1b\\pw\x1b]8;;\x1b\\", vec![]),
            (
                "\x1b]8;;javascript:alert(1)\x1b\\click\x1b]8;;\x1b\\",
                vec![],
            ),
            // An empty link text has nothing to anchor to on the row.
            ("\x1b]8;;https://a.com\x1b\\\x1b]8;;\x1b\\", vec![]),
            // A close with no open is not a link.
            ("\x1b]8;;\x1b\\plain", vec![]),
            ("no links here", vec![]),
            // Other OSC sequences are left alone.
            ("\x1b]0;Window Title\x07text", vec![]),
        ];
        for (input, expected) in cases {
            assert_eq!(extract_links(input.as_bytes()), expected, "{input:?}");
        }
    }

    /// Rows copied verbatim from a real Claude Code pane (v2.1.260). It wraps
    /// the target in an `id=` param and closes the run with the color reset
    /// INSIDE the hyperlink, so the captured text carries an SGR the rendered
    /// row does not.
    #[test]
    fn extracts_real_claude_code_hyperlinks() {
        let cases = [
            // A markdown link in message text: the issue's own repro, printed
            // by asking the agent for `[the AoE repo](https://github.com/...)`.
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
            // Claude Code's own UI chrome, from its workspace-trust prompt.
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
        // An unterminated opening sequence carries no target at all.
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
