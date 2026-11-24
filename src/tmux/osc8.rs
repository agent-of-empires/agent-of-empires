//! OSC 8 hyperlinks: [`GridLinks`] keeps them on a vt100 grid's cells, which
//! vt100 itself cannot, and [`extract_links`] pairs each target in a frame's
//! text with the visible text it wraps.

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

/// A target the preview could open (`session::preview_link_openable`), so an
/// unopenable one never takes a table slot; a control byte could inject escapes.
fn usable_uri(uri: &str) -> bool {
    crate::session::preview_link_openable(uri) && !uri.chars().any(char::is_control)
}

struct Osc8Scanner {
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
    fn new() -> Self {
        Self { buf: Vec::new() }
    }

    fn feed(&mut self, chunk: &[u8]) -> Vec<PaneLink> {
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
    fn finish(&mut self) -> Option<PaneLink> {
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

/// Zero-width code points appended to every cell printed inside an OSC 8 run,
/// naming the run's target. vt100 keeps them as part of the cell's contents,
/// so the target moves with the cell through scrolling, erasure, insertion and
/// scrollback without a parallel position table. Variation Selectors
/// Supplement: zero width, and meaningless outside ideographic variation
/// sequences, which pane output does not carry (stray ones are dropped).
const MARK_BASE: u32 = 0xE0100;
const MARK_RADIX: u32 = 240;
/// Distinct targets one grid names at once; a run past this prints as plain
/// text until [`GridLinks::reclaim`] frees an id no cell names any more.
const MAX_GRID_LINKS: usize = 1024;
/// A reclaim that frees fewer ids than this skips the next this many
/// reclaims, so a flood of new targets cannot rescan the grid per link.
const RECLAIM_COOLDOWN: usize = MAX_GRID_LINKS / 16;
/// vt100 stops appending to a cell at 18 of its 22 bytes, and both marks
/// (8 bytes) must still fit behind the zero-width code points they follow.
const MARKED_CELL_LIMIT: usize = 14;

fn is_mark(c: char) -> bool {
    (MARK_BASE..MARK_BASE + MARK_RADIX).contains(&u32::from(c))
}

fn push_mark(out: &mut Vec<u8>, id: u16) {
    let id = u32::from(id);
    let mut buf = [0u8; 4];
    for digit in [id / MARK_RADIX, id % MARK_RADIX] {
        let c = char::from_u32(MARK_BASE + digit).expect("mark range is valid");
        out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
    }
}

/// A cell's visible text and the link id its marks name, if any.
pub(crate) fn split_cell(contents: &str) -> (std::borrow::Cow<'_, str>, Option<u16>) {
    if !contents.chars().any(is_mark) {
        return (std::borrow::Cow::Borrowed(contents), None);
    }
    let digits: Vec<u32> = contents
        .chars()
        .filter(|&c| is_mark(c))
        .map(|c| u32::from(c) - MARK_BASE)
        .collect();
    let id = match digits[..] {
        [.., hi, lo] => u16::try_from(hi * MARK_RADIX + lo).ok(),
        _ => None,
    };
    let text: String = contents.chars().filter(|&c| !is_mark(c)).collect();
    (std::borrow::Cow::Owned(text), id)
}

/// OSC 8 state for one vt100 grid, stored as the parser's callbacks so a
/// reseed that replaces the parser replaces this with it.
///
/// [`Self::mark`] runs the stream through its own `vte` parser to find each
/// glyph printed inside a hyperlink and appends that link's mark after the
/// glyph and any zero-width code points joining its cell. vt100 then stores
/// the mark in the same cell.
#[derive(Default)]
pub(crate) struct GridLinks {
    vte: vte::Parser,
    scan: LinkScan,
}

struct Target {
    uri: String,
    /// Whether the primary and the alternate grid may hold a cell naming it.
    on: [bool; 2],
}

/// What the byte just fed completed.
#[derive(Default, Clone, Copy)]
enum Event {
    #[default]
    None,
    /// A glyph vt100 draws into a new cell, of this many UTF-8 bytes.
    Glyph(usize),
    /// A zero-width code point vt100 appends to the previous cell.
    ZeroWidth(usize),
    /// A mark code point arriving from the pane, to drop.
    Stray,
    /// A control, escape, or OSC, after which a later zero-width code point
    /// may land on another cell.
    Other,
}

#[derive(Default)]
struct LinkScan {
    active: Option<u16>,
    targets: Vec<Option<Target>>,
    free: Vec<u16>,
    ids: std::collections::HashMap<String, u16>,
    /// The alternate grid is the one being drawn.
    alt: bool,
    /// A target that found the table full, waiting on [`GridLinks::reclaim`].
    starved: Option<String>,
    cooldown: usize,
    event: Event,
}

impl LinkScan {
    fn intern(&mut self, uri: &str) -> Option<u16> {
        if let Some(&id) = self.ids.get(uri) {
            return Some(id);
        }
        let id = match self.free.pop() {
            Some(id) => id,
            None if self.targets.len() < MAX_GRID_LINKS => {
                self.targets.push(None);
                u16::try_from(self.targets.len() - 1).ok()?
            }
            None => return None,
        };
        self.targets[usize::from(id)] = Some(Target {
            uri: uri.to_string(),
            on: [false; 2],
        });
        self.ids.insert(uri.to_string(), id);
        Some(id)
    }

    fn open(&mut self, uri: &str) -> Option<u16> {
        let id = self.intern(uri);
        if id.is_none() {
            if self.cooldown == 0 {
                self.starved = Some(uri.to_string());
            } else {
                self.cooldown -= 1;
            }
        }
        id
    }
}

impl vte::Perform for LinkScan {
    fn print(&mut self, c: char) {
        if is_mark(c) {
            self.event = Event::Stray;
            return;
        }
        // Mirrors which code points vt100 draws into a new cell.
        if c == '\u{fffd}' || ('\u{80}'..'\u{a0}').contains(&c) {
            return;
        }
        if unicode_width::UnicodeWidthChar::width(c).unwrap_or(1) == 0 {
            self.event = Event::ZeroWidth(c.len_utf8());
            return;
        }
        self.event = Event::Glyph(c.len_utf8());
        let grid = usize::from(self.alt);
        if let Some(Some(target)) = self.active.map(|id| &mut self.targets[usize::from(id)]) {
            target.on[grid] = true;
        }
    }

    fn execute(&mut self, _byte: u8) {
        self.event = Event::Other;
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {
        self.event = Event::Other;
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        _ignore: bool,
        action: char,
    ) {
        self.event = Event::Other;
        if intermediates != b"?" || !matches!(action, 'h' | 'l') {
            return;
        }
        // The modes vt100 switches grids on; 1049 clears the alternate grid.
        for param in params {
            match (param, action) {
                ([47], _) => self.alt = action == 'h',
                ([1049], 'h') => {
                    self.alt = true;
                    for target in self.targets.iter_mut().flatten() {
                        target.on[1] = false;
                    }
                }
                ([1049], _) => self.alt = false,
                _ => {}
            }
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        self.event = Event::Other;
        let [b"8", _, uri @ ..] = params else {
            return;
        };
        // vte splits on every `;`, including any inside the URI.
        let uri = uri.join(&b';');
        self.active = match std::str::from_utf8(&uri) {
            Ok("") => None,
            Ok(uri) if uri.len() <= MAX_URI && usable_uri(uri) => self.open(uri),
            _ => None,
        };
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.event = Event::Other;
        // RIS resets the terminal, hyperlink and both grids included.
        if intermediates.is_empty() && byte == b'c' {
            self.active = None;
            self.alt = false;
            for target in self.targets.iter_mut().flatten() {
                target.on = [false; 2];
            }
        }
    }
}

impl vt100::Callbacks for GridLinks {}

/// A glyph's mark not yet written: it goes at byte `at` of the chunk, after
/// whatever zero-width code points joined the glyph's `cell` bytes.
struct Owed {
    id: u16,
    at: usize,
    cell: usize,
}

impl GridLinks {
    /// The leading `.1` bytes of `bytes` with a mark after each glyph printed
    /// inside a hyperlink, ready for the grid's own `process`. Fewer than all
    /// bytes when a target found the table full: process them, then call
    /// [`Self::reclaim`] and mark the rest. Borrows when nothing needed marking.
    pub(crate) fn mark<'a>(&mut self, bytes: &'a [u8]) -> (std::borrow::Cow<'a, [u8]>, usize) {
        let mut out: Option<Vec<u8>> = None;
        let mut flushed = 0;
        let mut owed: Option<Owed> = None;
        let settle = |out: &mut Option<Vec<u8>>, flushed: &mut usize, owed: Option<Owed>| {
            if let Some(owed) = owed {
                let out = out.get_or_insert_with(|| Vec::with_capacity(bytes.len() + 64));
                out.extend_from_slice(&bytes[*flushed..owed.at]);
                push_mark(out, owed.id);
                *flushed = owed.at;
            }
        };
        let mut used = bytes.len();
        for i in 0..bytes.len() {
            self.vte.advance(&mut self.scan, &bytes[i..=i]);
            match std::mem::take(&mut self.scan.event) {
                Event::None => {}
                // vt100 rechecks a cell's width on U+FE0F only when it follows
                // the glyph directly, so the mark waits behind it.
                Event::ZeroWidth(len) => match owed.as_mut() {
                    Some(o) if o.cell + len < MARKED_CELL_LIMIT => {
                        o.at = i + 1;
                        o.cell += len;
                    }
                    _ => settle(&mut out, &mut flushed, owed.take()),
                },
                Event::Glyph(len) => {
                    settle(&mut out, &mut flushed, owed.take());
                    owed = self.scan.active.map(|id| Owed {
                        id,
                        at: i + 1,
                        cell: len,
                    });
                }
                Event::Stray => {
                    settle(&mut out, &mut flushed, owed.take());
                    let out = out.get_or_insert_with(|| Vec::with_capacity(bytes.len() + 64));
                    // A mark is four UTF-8 bytes; one split across reads stays.
                    out.extend_from_slice(&bytes[flushed..(i + 1).saturating_sub(4).max(flushed)]);
                    flushed = i + 1;
                }
                Event::Other => settle(&mut out, &mut flushed, owed.take()),
            }
            if self.scan.starved.is_some() {
                used = i + 1;
                break;
            }
        }
        settle(&mut out, &mut flushed, owed);
        let marked = match out {
            None => std::borrow::Cow::Borrowed(&bytes[..used]),
            Some(mut out) => {
                out.extend_from_slice(&bytes[flushed..used]);
                std::borrow::Cow::Owned(out)
            }
        };
        (marked, used)
    }

    /// A target is waiting for [`Self::reclaim`].
    pub(crate) fn starved(&self) -> bool {
        self.scan.starved.is_some()
    }

    /// Free every id that no cell of either grid can name, given `live`, the
    /// ids the drawn grid's cells (scrollback included) name now, then open
    /// the starved target. The hidden grid's ids stay held until it is drawn.
    pub(crate) fn reclaim(&mut self, live: &std::collections::HashSet<u16>) {
        let scan = &mut self.scan;
        let grid = usize::from(scan.alt);
        let mut freed = 0;
        for (id, slot) in scan.targets.iter_mut().enumerate() {
            let Some(target) = slot else {
                continue;
            };
            let id = u16::try_from(id).expect("ids fit u16");
            target.on[grid] = live.contains(&id);
            if target.on == [false; 2] && scan.active != Some(id) {
                scan.ids.remove(&target.uri);
                *slot = None;
                scan.free.push(id);
                freed += 1;
            }
        }
        if freed < RECLAIM_COOLDOWN {
            scan.cooldown = RECLAIM_COOLDOWN;
        }
        if let Some(uri) = scan.starved.take() {
            scan.active = scan.intern(&uri);
        }
    }

    pub(crate) fn uri(&self, id: u16) -> Option<&str> {
        let target = self.scan.targets.get(usize::from(id))?.as_ref()?;
        Some(&target.uri)
    }
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
    #[serial_test::serial]
    fn extracts_across_forms_and_neighbours() {
        let cases: Vec<(&str, Vec<PaneLink>)> = vec![
            (
                "\x1b]8;;https://example.com\x1b\\Click Here\x1b]8;;\x1b\\",
                vec![link("Click Here", "https://example.com")],
            ),
            // A link whose close never arrives is still emitted at the end.
            (
                "\x1b]8;;https://example.com\x1b\\Click Here",
                vec![link("Click Here", "https://example.com")],
            ),
            ("\x1b]8;;https://example.com", vec![]),
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
            // With no `session.preview_link_prefixes`, only web targets are kept.
            ("\x1b]8;;file:///etc/passwd\x1b\\pw\x1b]8;;\x1b\\", vec![]),
            (
                "\x1b]8;;vscode://file/Users/x/notes/a.md:12\x1b\\notes/a.md\x1b]8;;\x1b\\",
                vec![],
            ),
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
