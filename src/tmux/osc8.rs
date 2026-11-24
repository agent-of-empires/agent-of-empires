//! OSC 8 hyperlinks: [`GridLinks`] keeps them on a vt100 grid's cells, which
//! vt100 itself cannot, and [`row_links`] finds each run a frame's rows carry.

/// One OSC 8 run on a frame row, positioned by `char` offset into the row's
/// text with escapes removed, so it matches the rendered row cell for cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RowLink {
    pub(crate) row: usize,
    pub(crate) start: usize,
    pub(crate) text: String,
    pub(crate) uri: String,
}

const OSC8_OPEN: &[u8] = b"\x1b]8;";

const MAX_URI: usize = 2048;

/// A target the preview could open (`session::preview_link_openable`), so an
/// unopenable one never takes a table slot; a control byte could inject escapes.
fn usable_uri(uri: &str) -> bool {
    crate::session::preview_link_openable(uri) && !uri.chars().any(char::is_control)
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

/// The OSC 8 runs in `content`, by row (rows split on `\n`), in order. A run
/// still open at a row's end carries onto the next row, as on a terminal.
pub(crate) fn row_links(content: &str) -> Vec<RowLink> {
    let mut out = Vec::new();
    let mut active: Option<String> = None;
    for (row, line) in content.split('\n').enumerate() {
        let mut rest = line.as_bytes();
        let mut col = 0;
        loop {
            let at = find(rest, OSC8_OPEN);
            let text = crate::tmux::utils::strip_ansi(&String::from_utf8_lossy(
                &rest[..at.unwrap_or(rest.len())],
            ));
            if let Some(uri) = active.as_ref().filter(|uri| usable_uri(uri)) {
                let trimmed = text.trim_start();
                let lead = text.len() - trimmed.len();
                let trimmed = trimmed.trim_end();
                if !trimmed.is_empty() {
                    out.push(RowLink {
                        row,
                        start: col + text[..lead].chars().count(),
                        text: trimmed.to_string(),
                        uri: uri.clone(),
                    });
                }
            }
            col += text.chars().count();
            let Some(at) = at else {
                break;
            };
            rest = &rest[at..];
            match parse_seq(rest) {
                Seq::Found { uri, consumed } => {
                    active = (!uri.is_empty()).then_some(uri);
                    rest = &rest[consumed..];
                }
                Seq::Skip(n) => rest = &rest[n..],
                Seq::Incomplete => break,
            }
        }
    }
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
/// Supplement: zero width, and a cell holds at most one of the pane's own,
/// right after its base, so a cell's last two name its link.
const MARK_BASE: u32 = 0xE0100;
const MARK_RADIX: u32 = 240;
/// Distinct targets one grid names at once; a run past this prints as plain
/// text until [`GridLinks::reclaim`] frees an id no cell names any more.
const MAX_GRID_LINKS: usize = 1024;
/// A reclaim that frees fewer ids than this skips the next this many
/// reclaims, so a flood of new targets cannot rescan the grid per link.
const RECLAIM_COOLDOWN: usize = MAX_GRID_LINKS / 16;
/// vt100 stops appending to a cell at this many bytes.
const CELL_APPEND_LIMIT: usize = 18;
/// Both marks (8 bytes) fit only in a cell holding fewer bytes than this; a
/// fuller cell keeps its text and goes unlinked.
const MARKED_CELL_LIMIT: usize = CELL_APPEND_LIMIT - 4;
/// Bytes of an OSC kept for [`LinkScan::osc`]; a longer one names no target.
const MAX_OSC: usize = MAX_URI + 256;
/// Bytes after an owed mark one read may hold back for the next, room for an
/// SGR, an OSC 8 close and the next run's opener.
const MAX_HELD: usize = MAX_OSC + 64;

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
    let marks = contents.chars().filter(|&c| is_mark(c)).count();
    // None, or the pane's own selector.
    if marks < 2 {
        return (std::borrow::Cow::Borrowed(contents), None);
    }
    let mut seen = 0;
    let mut text = String::with_capacity(contents.len());
    let mut id = 0;
    for c in contents.chars() {
        if is_mark(c) {
            seen += 1;
            if seen > marks - 2 {
                id = id * MARK_RADIX + (u32::from(c) - MARK_BASE);
                continue;
            }
        }
        text.push(c);
    }
    (std::borrow::Cow::Owned(text), u16::try_from(id).ok())
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
    /// The last read ended on a linked cell zero-width code points from the
    /// next read may still join (U+FE0F must follow its glyph directly).
    owed: Option<Owed>,
    /// The bytes after `owed`'s glyph, not yet given to the grid.
    held: Vec<u8>,
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
    /// An SGR, OSC or ST: the cursor stays, so a zero-width code point after
    /// it still joins the cell before it. Owed marks wait past it.
    Quiet,
    /// Any other control or escape, after which a zero-width code point may
    /// land on another cell.
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
    /// The last event was a glyph, so a selector arriving now is the pane's.
    after_glyph: bool,
    /// The bytes from the last ESC, so an OSC reads its whole payload: vte
    /// keeps 16 parameters and drops the rest.
    osc: Vec<u8>,
    /// `osc` outgrew [`MAX_OSC`].
    osc_cut: bool,
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

    fn keep_osc_byte(&mut self, byte: u8) {
        if byte == 0x1b {
            self.osc.clear();
            self.osc_cut = false;
        } else if self.osc.len() >= MAX_OSC {
            self.osc_cut = true;
            return;
        }
        self.osc.push(byte);
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
            // One selector may follow its base; any other could forge a mark.
            self.event = if self.after_glyph {
                Event::ZeroWidth(c.len_utf8())
            } else {
                Event::Stray
            };
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
        self.event = if action == 'm' {
            Event::Quiet
        } else {
            Event::Other
        };
        // vt100 picks its private-mode handler by the first intermediate alone.
        if intermediates.first() != Some(&b'?') || !matches!(action, 'h' | 'l') {
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
        self.event = Event::Quiet;
        let [b"8", _, ..] = params else {
            return;
        };
        // vte drops the C0 controls inside an OSC, so they are no part of it.
        let payload: Vec<u8> = match self.osc.split_first() {
            Some((0x1b, rest)) if !self.osc_cut => {
                rest.iter().copied().filter(|&b| b >= 0x20).collect()
            }
            _ => Vec::new(),
        };
        let uri = payload.strip_prefix(b"]8;").and_then(|rest| {
            rest.iter()
                .position(|&b| b == b';')
                .map(|at| &rest[at + 1..])
        });
        let Some(uri) = uri else {
            self.active = None;
            return;
        };
        self.active = match std::str::from_utf8(uri) {
            Ok("") => None,
            Ok(uri) if uri.len() <= MAX_URI && usable_uri(uri) => self.open(uri),
            _ => None,
        };
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        self.event = if intermediates.is_empty() && byte == b'\\' {
            Event::Quiet
        } else {
            Event::Other
        };
        // RIS resets the terminal, hyperlink and both grids included.
        if intermediates.is_empty() && byte == b'c' {
            self.active = None;
            self.alt = false;
            self.cooldown = 0;
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
    /// `bytes` with a mark after each glyph printed inside a hyperlink, ready
    /// for the grid's own `process`, and how many of `bytes` it covers. Fewer
    /// than all when a target found the table full: process them, then call
    /// [`Self::reclaim`] and mark the rest. Borrows when nothing needed marking.
    ///
    /// A read ending on a linked glyph, or on escapes that leave the cursor
    /// after it, holds the mark (and those bytes) for the next read, since a
    /// zero-width code point there still joins that cell. [`Self::flush`]
    /// gives them up when no read follows.
    pub(crate) fn mark<'a>(&mut self, bytes: &'a [u8]) -> (std::borrow::Cow<'a, [u8]>, usize) {
        let held = std::mem::take(&mut self.held);
        let skip = held.len();
        let input: std::borrow::Cow<'a, [u8]> = if held.is_empty() {
            std::borrow::Cow::Borrowed(bytes)
        } else {
            std::borrow::Cow::Owned([held.as_slice(), bytes].concat())
        };
        let mut out: Option<Vec<u8>> = None;
        let mut flushed = 0;
        let mut owed = self.owed.take();
        let settle = |out: &mut Option<Vec<u8>>, flushed: &mut usize, owed: Option<Owed>| {
            if let Some(owed) = owed.filter(|o| o.cell < MARKED_CELL_LIMIT) {
                let out = out.get_or_insert_with(|| Vec::with_capacity(input.len() + 64));
                out.extend_from_slice(&input[*flushed..owed.at]);
                push_mark(out, owed.id);
                *flushed = owed.at;
            }
        };
        let mut used = input.len();
        for i in skip..input.len() {
            self.vte.advance(&mut self.scan, &input[i..=i]);
            self.scan.keep_osc_byte(input[i]);
            let event = std::mem::take(&mut self.scan.event);
            if !matches!(event, Event::None) {
                self.scan.after_glyph = matches!(event, Event::Glyph(_));
            }
            match event {
                // vt100 rechecks a cell's width on U+FE0F only when it follows
                // the glyph directly, so the mark waits behind it.
                Event::ZeroWidth(len) => {
                    if let Some(o) = owed.as_mut() {
                        o.at = i + 1;
                        if o.cell < CELL_APPEND_LIMIT {
                            o.cell += len;
                        }
                    }
                }
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
                    let out = out.get_or_insert_with(|| Vec::with_capacity(input.len() + 64));
                    // A mark is four UTF-8 bytes; one split across reads stays.
                    out.extend_from_slice(&input[flushed..(i + 1).saturating_sub(4).max(flushed)]);
                    flushed = i + 1;
                }
                Event::None | Event::Quiet => {}
                Event::Other => settle(&mut out, &mut flushed, owed.take()),
            }
            if self.scan.starved.is_some() {
                used = i + 1;
                break;
            }
        }
        let consumed = used - skip;
        // Only escapes that leave the cursor put, or partial ones, follow an
        // owed mark's `at`, so the next read's combining marks still join it.
        if let Some(o) = owed.take_if(|o| used == input.len() && used - o.at <= MAX_HELD) {
            self.held = input[o.at..used].to_vec();
            used = o.at;
            self.owed = Some(Owed { at: 0, ..o });
        }
        settle(&mut out, &mut flushed, owed);
        let marked = match out {
            None if skip == 0 => std::borrow::Cow::Borrowed(&bytes[..used]),
            None => std::borrow::Cow::Owned(input[..used].to_vec()),
            Some(mut out) => {
                out.extend_from_slice(&input[flushed..used]);
                std::borrow::Cow::Owned(out)
            }
        };
        (marked, consumed)
    }

    /// The mark and bytes held for a next read, for output that may not come.
    pub(crate) fn flush(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        if let Some(o) = self.owed.take().filter(|o| o.cell < MARKED_CELL_LIMIT) {
            push_mark(&mut out, o.id);
        }
        out.append(&mut self.held);
        out
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

    fn found(content: &str) -> Vec<(usize, usize, String, String)> {
        row_links(content)
            .into_iter()
            .map(|l| (l.row, l.start, l.text, l.uri))
            .collect()
    }

    fn owned(want: &[(usize, usize, &str, &str)]) -> Vec<(usize, usize, String, String)> {
        want.iter()
            .map(|&(row, start, text, uri)| (row, start, text.into(), uri.into()))
            .collect()
    }

    #[test]
    #[serial_test::serial]
    fn finds_each_run_at_its_row_and_column() {
        type Case = (
            &'static str,
            Vec<(usize, usize, &'static str, &'static str)>,
        );
        let cases: Vec<Case> = vec![
            (
                "\x1b]8;;https://example.com\x1b\\Click Here\x1b]8;;\x1b\\",
                vec![(0, 0, "Click Here", "https://example.com")],
            ),
            // A run whose close never arrives still counts.
            (
                "\x1b]8;;https://example.com\x1b\\Click Here",
                vec![(0, 0, "Click Here", "https://example.com")],
            ),
            ("\x1b]8;;https://example.com", vec![]),
            (
                "before \x1b]8;;https://a.com\x1b\\text\x1b]8;;\x1b\\ after",
                vec![(0, 7, "text", "https://a.com")],
            ),
            (
                "\x1b]8;id=abc;https://a.com\x1b\\A\x1b]8;;\x1b\\",
                vec![(0, 0, "A", "https://a.com")],
            ),
            (
                "\x1b]8;;https://a.com\x07A\x1b]8;;\x07",
                vec![(0, 0, "A", "https://a.com")],
            ),
            (
                "\x1b[1m>\x1b[0m \x1b]8;;https://a.com\x1b\\\x1b[32m green\x1b[0m\x1b]8;;\x1b\\",
                vec![(0, 3, "green", "https://a.com")],
            ),
            (
                "\x1b]8;;https://a.com\x1b\\A\x1b]8;;https://b.com\x1b\\B\x1b]8;;\x1b\\",
                vec![(0, 0, "A", "https://a.com"), (0, 1, "B", "https://b.com")],
            ),
            // The same label on two rows is two runs, each with its own target.
            (
                "\x1b]8;;https://a.com\x1b\\docs\x1b]8;;\x1b\\\n  \x1b]8;;https://b.com\x1b\\docs\x1b]8;;\x1b\\",
                vec![(0, 0, "docs", "https://a.com"), (1, 2, "docs", "https://b.com")],
            ),
            // Columns count characters, not bytes.
            (
                "\u{6f22}\u{5b57} \x1b]8;;https://a.com\x1b\\x\x1b]8;;\x1b\\",
                vec![(0, 3, "x", "https://a.com")],
            ),
            // With no `session.preview_link_prefixes`, only web targets are kept.
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
            assert_eq!(found(input), owned(&expected), "{input:?}");
        }
    }

    #[test]
    fn finds_real_claude_code_hyperlinks() {
        let cases = [
            (
                concat!(
                    "\x1b[38;5;231m\x1b[49m\u{25cf}\x1b[39m \x1b[94m",
                    "\x1b]8;id=1nl9mmd;https://github.com/agent-of-empires/agent-of-empires\x1b\\",
                    "the AoE repo\x1b[39m\x1b]8;;\x1b\\"
                ),
                (
                    0,
                    2,
                    "the AoE repo",
                    "https://github.com/agent-of-empires/agent-of-empires",
                ),
            ),
            (
                concat!(
                    " \x1b[38;5;246m\x1b]8;id=zaxmda;https://code.claude.com/docs/en/security\x1b\\",
                    "Security guide\x1b[39m\x1b]8;;\x1b\\"
                ),
                (
                    0,
                    1,
                    "Security guide",
                    "https://code.claude.com/docs/en/security",
                ),
            ),
        ];
        for (row, expected) in cases {
            assert_eq!(found(row), owned(&[expected]), "{row:?}");
        }
    }

    #[test]
    fn a_pane_selector_stays_text_beside_a_link_mark() {
        let ideograph = "\u{845b}\u{E0101}";
        assert_eq!(split_cell(ideograph), (ideograph.into(), None));
        let mut marked = ideograph.as_bytes().to_vec();
        push_mark(&mut marked, 300);
        let marked = String::from_utf8(marked).unwrap();
        assert_eq!(split_cell(&marked), (ideograph.into(), Some(300)));
    }
}
