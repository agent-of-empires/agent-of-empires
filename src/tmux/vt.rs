//! Shared in-process VT channel.
//!
//! A `tmux pipe-pane -IO` stream feeds a pane's raw output into an in-process
//! [`vt100::Parser`] (a real grid: alt-screen buffer, cursor, mouse/DEC modes),
//! and the same full-duplex unix socket carries keystroke bytes back to the
//! pane. tmux still owns the pane (process, persistence, kill-tree); only the
//! live render/input transport lives here.
//!
//! One [`VtChannel`] per tmux session, shared and refcounted: every viewer (the
//! native TUI live preview and the web/mobile live terminal) holds an `Arc`, so
//! a session is parsed once no matter how many surfaces watch it. The channel
//! tears down (disables the pipe, stops the forwarder) when the last `Arc`
//! drops. Unix-only; the whole module is `#[cfg(unix)]`.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use base64::Engine;

use crate::tmux::PaneCursor;

/// Largest base64 payload an OSC 52 sequence may carry before the scanner
/// abandons it (the TUI-side `copy_to_clipboard` truncates at 1 MiB of raw
/// bytes anyway, and an unbounded accumulator would let a malformed stream
/// grow it forever).
const OSC52_MAX_PAYLOAD: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Osc52State {
    /// Searching for the next ESC.
    Idle,
    /// Seen `ESC`.
    Esc,
    /// Seen `ESC ]`.
    OscStart,
    /// Seen `ESC ] 5`.
    Five,
    /// Seen `ESC ] 5 2`.
    Two,
    /// Inside the selection-target params (`c`, `p`, ...), up to the `;`
    /// that opens the payload.
    Params,
    /// Accumulating the base64 payload.
    Payload,
    /// Seen `ESC` inside the payload: either the opening of an ST
    /// terminator (`ESC \`) or, in a tmux-passthrough-wrapped sequence,
    /// the first half of a doubled `ESC ESC \`.
    PayloadEsc,
}

/// Incremental OSC 52 clipboard-write extractor for the raw pane stream.
///
/// The wrapped agent's "copy" comes out of the pane as
/// `ESC ] 52 ; <targets> ; <base64> BEL|ST` (possibly tmux-passthrough
/// wrapped, which doubles the inner ESCs). The stream arrives in arbitrary
/// read-sized chunks, so the scanner is a per-byte state machine that
/// carries its state across `feed` calls; a sequence split at any byte
/// boundary still extracts.
///
/// Query (`?`) and empty payloads are skipped: a query is a read request,
/// and forwarding an empty write would *clear* the host clipboard, which is
/// never what a dropped or malformed copy should do.
struct Osc52Scanner {
    state: Osc52State,
    params_len: usize,
    payload: Vec<u8>,
}

impl Osc52Scanner {
    fn new() -> Self {
        Self {
            state: Osc52State::Idle,
            params_len: 0,
            payload: Vec::new(),
        }
    }

    /// Scan one chunk; returns the decoded text of the last complete
    /// non-empty clipboard write it contains, if any.
    fn feed(&mut self, chunk: &[u8]) -> Option<String> {
        use Osc52State::*;
        let mut found = None;
        for &b in chunk {
            self.state = match (self.state, b) {
                (Idle, 0x1b) => Esc,
                (Idle, _) => Idle,
                (Esc, b']') => OscStart,
                (OscStart, b'5') => Five,
                (Five, b'2') => Two,
                (Two, b';') => {
                    self.params_len = 0;
                    Params
                }
                (Params, b';') => {
                    self.payload.clear();
                    Payload
                }
                (Params, 0x07) => Idle,
                (Params, 0x1b) => Esc,
                (Params, _) => {
                    // The targets field is a handful of selection letters;
                    // anything longer is not an OSC 52 we understand.
                    self.params_len += 1;
                    if self.params_len > 16 {
                        Idle
                    } else {
                        Params
                    }
                }
                (Payload, 0x07) => {
                    if let Some(text) = self.complete() {
                        found = Some(text);
                    }
                    Idle
                }
                (Payload, 0x1b) => PayloadEsc,
                (Payload, c) if is_payload_byte(c) => {
                    if self.payload.len() >= OSC52_MAX_PAYLOAD {
                        Idle
                    } else {
                        self.payload.push(c);
                        Payload
                    }
                }
                (Payload, _) => Idle,
                (PayloadEsc, b'\\') => {
                    if let Some(text) = self.complete() {
                        found = Some(text);
                    }
                    Idle
                }
                // A tmux-passthrough-wrapped sequence doubles inner ESCs,
                // so its ST arrives as `ESC ESC \`.
                (PayloadEsc, 0x1b) => PayloadEsc,
                (PayloadEsc, _) => Idle,
                // Any non-matching byte after a bare ESC: restart if it is
                // itself an ESC (`ESC ESC ]` from tmux passthrough doubling),
                // else fall back to searching.
                (Esc | OscStart | Five | Two, 0x1b) => Esc,
                (Esc | OscStart | Five | Two, _) => Idle,
            };
        }
        found
    }

    /// Decode the accumulated payload; `None` for queries, empty writes,
    /// and undecodable base64.
    fn complete(&mut self) -> Option<String> {
        let payload = std::mem::take(&mut self.payload);
        if payload.is_empty() || payload.contains(&b'?') {
            return None;
        }
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&payload)
            .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&payload))
            .ok()?;
        if decoded.is_empty() {
            return None;
        }
        Some(String::from_utf8_lossy(&decoded).into_owned())
    }
}

/// Bytes legal inside the OSC 52 payload: base64 plus `?` (a clipboard
/// query, recognised so the sequence parses to completion and is then
/// skipped rather than aborting mid-sequence).
fn is_payload_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'?')
}

/// `aoe __vt-pipe <socket>`: the bidirectional `pipe-pane -IO` forwarder. tmux
/// connects the pane's OUTPUT to this process's stdin and the pane's INPUT to
/// its stdout, so:
///   - stdin (pane output) -> socket  (a viewer reads it into a vt100 grid)
///   - socket -> stdout (pane input)  (a viewer writes keystrokes, no fork)
///
/// One full-duplex unix socket carries both directions. Unbuffered: direct
/// `write(2)` per chunk so a keystroke is not stalled behind a stdio buffer.
pub(crate) fn run_pipe(socket: &str) -> std::io::Result<()> {
    use std::io::Write;
    let sock_r = UnixStream::connect(socket)?;
    let mut sock_w = sock_r.try_clone()?;

    // stdin (pane output) -> socket
    let pump_out = std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = [0u8; 8192];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if sock_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = sock_w.shutdown(std::net::Shutdown::Write);
    });

    // socket -> stdout (pane input)
    let mut sock_r = sock_r;
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 4096];
    loop {
        match sock_r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).is_err() {
                    break;
                }
                let _ = stdout.flush();
            }
            Err(_) => break,
        }
    }
    let _ = pump_out.join();
    Ok(())
}

/// Live channels keyed by tmux session name, held weakly so the entry vanishes
/// once the last viewer drops its `Arc`. `acquire` upgrades or re-arms.
static REGISTRY: LazyLock<Mutex<HashMap<String, Weak<VtChannel>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SOCK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Monotonic base for chunk-arrival timestamps. The reader stamps each chunk's
/// arrival against this (millis), and the TUI capture worker reads the deltas
/// via [`VtChannel::chunk_timing`] to drive its repaint-quiescence debounce.
static CHUNK_CLOCK: LazyLock<Instant> = LazyLock::new(Instant::now);

fn chunk_now_ms() -> u64 {
    CHUNK_CLOCK.elapsed().as_millis() as u64
}

/// A `(Mutex, Condvar)` pair an in-process poller parks on. Registered via
/// [`VtChannel::set_change_wakeup`]; the reader thread pokes it after every
/// grid change (and on death) so the poller samples the moment output lands
/// instead of after the remainder of a fixed poll interval. Web viewers use
/// the tokio `watch` channel instead; this is the std-thread equivalent for
/// the TUI capture worker.
pub(crate) type ChangeWakeup = Arc<(Mutex<()>, Condvar)>;

/// Poke a registered change wakeup, if any. The slot lock is held only to
/// clone the pair; the pair's own mutex is then taken so the notify
/// serializes with a parker between its `lock` and `wait` (otherwise the
/// wake could fire into the gap and be lost).
fn notify_change_wakeup(slot: &Mutex<Option<ChangeWakeup>>) {
    let pair = match slot.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    if let Some(pair) = pair {
        if let Ok(_g) = pair.0.lock() {
            pair.1.notify_one();
        }
    }
}

/// Lines of scrollback the grid keeps, and how much history the seed pulls from
/// the pane. Matches tmux's default `history-limit` so a freshly armed channel
/// (e.g. after switching away from a session and back) has the pane's history
/// immediately, not just the visible screen.
pub(crate) const SCROLLBACK_LINES: usize = 2000;

fn lookup(session: &str) -> Option<Arc<VtChannel>> {
    REGISTRY
        .lock()
        .unwrap()
        .get(session)
        .and_then(Weak::upgrade)
}

/// If `session` has a *live* armed channel, return its current cursor-key mode
/// (DECCKM): `Some(true)` = application cursor keys (`ESC O A`), `Some(false)` =
/// normal (`ESC [ A`). `None` means no channel is armed, or its forwarder has
/// disconnected. Presence of `Some` is the single-writer signal: while live,
/// ALL pane input must go through [`try_send_input`] (never `send-keys`), so
/// the two writers don't interleave. Gating on liveness means a dead channel
/// reports `None` and input falls back to `send-keys` rather than vanishing.
pub(crate) fn input_mode(session: &str) -> Option<bool> {
    lookup(session)
        .filter(|c| c.is_alive())
        .map(|c| c.app_cursor.load(Ordering::Relaxed))
}

/// Deliver raw `bytes` to `session`'s pane via its channel. Returns `true` if
/// written, `false` if no channel is armed or the forwarder hasn't connected.
pub(crate) fn try_send_input(session: &str, bytes: &[u8]) -> bool {
    lookup(session)
        .map(|c| c.write_input(bytes))
        .unwrap_or(false)
}

/// Single-quote a path for the `/bin/sh -c` line `tmux pipe-pane` runs.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn pane_size(target: &str) -> Option<(u16, u16)> {
    let out = crate::tmux::tmux_command()
        .args([
            "display-message",
            "-p",
            "-t",
            target,
            "-F",
            "#{pane_width} #{pane_height}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.split_whitespace();
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    Some((w, h))
}

/// The pane state a seed needs that `capture-pane -e` can't carry: the terminal
/// modes the wheel-forward / scroll logic keys off, plus the real cursor
/// position and DECTCEM (show/hide) flag. `capture-pane` returns cell text and
/// SGR only, so without these the seeded parser has default modes and its cursor
/// stranded wherever the last replayed glyph ended (issue #2902).
#[derive(Clone, Copy, Default)]
struct PaneSeedState {
    alt: bool,
    mouse: bool,
    mouse_sgr: bool,
    /// `#{mouse_all_flag}`: any-event tracking (DEC 1003), which the hover
    /// forwarding keys off (#2904).
    mouse_all: bool,
    /// Cursor column / row in the pane's *visible-screen* coordinates (0-based),
    /// straight from tmux `#{cursor_x}` / `#{cursor_y}`.
    cursor_x: u16,
    cursor_y: u16,
    /// `#{cursor_flag}`: whether the app is showing the hardware cursor.
    cursor_visible: bool,
}

/// Query the pane's seed state in one `display-message` round-trip (the live
/// path is fork-sensitive, #2822, so modes and cursor share a single call).
fn pane_seed_state(target: &str) -> Option<PaneSeedState> {
    let out = crate::tmux::tmux_command()
        .args([
            "display-message",
            "-p",
            "-t",
            target,
            "-F",
            "#{alternate_on} #{mouse_any_flag} #{mouse_sgr_flag} #{mouse_all_flag} #{cursor_x} #{cursor_y} #{cursor_flag}",
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let mut it = s.split_whitespace();
    let alt = it.next().map(|f| f != "0").unwrap_or(false);
    let mouse = it.next().map(|f| f != "0").unwrap_or(false);
    let mouse_sgr = it.next().map(|f| f != "0").unwrap_or(false);
    let mouse_all = it.next().map(|f| f != "0").unwrap_or(false);
    let cursor_x = it.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let cursor_y = it.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let cursor_visible = it.next().map(|f| f != "0").unwrap_or(true);
    Some(PaneSeedState {
        alt,
        mouse,
        mouse_sgr,
        mouse_all,
        cursor_x,
        cursor_y,
        cursor_visible,
    })
}

/// Translate bare LF to CRLF so `capture-pane` seed rows (LF-separated) each
/// start at column 0 in the parser instead of staircasing off the previous
/// row's end column. An existing CR is left alone, so a stream that already
/// uses CRLF is unchanged. `capture-pane` never emits CR, so in practice this
/// just inserts one before each LF.
fn lf_to_crlf(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + raw.len() / 40 + 8);
    let mut prev = 0u8;
    for &b in raw {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

/// (Re)build `parser` from tmux's authoritative `capture-pane` at `cols`x`rows`,
/// resetting any prior content. `pipe-pane` carries only the app's incremental
/// output, never tmux's reflow, so on a resize a grid that merely `set_size`d
/// itself would keep its pre-resize layout while the app reprints onto it,
/// duplicating the prompt and stranding the cursor on the wrong row (the app
/// may never emit anything else, so the divergence is permanent). Rebuilding
/// from `capture-pane` re-syncs the grid to tmux exactly.
///
/// The seed is rendered content (`capture-pane -e`), so it carries no DEC
/// private-mode SETs, no cursor position, and no DECTCEM state. The pane's
/// modes, cursor, and hide flag are queried once ([`pane_seed_state`]) and
/// woven into the byte stream by [`assemble_seed_stream`].
fn seed_parser(
    target: &str,
    parser: &Mutex<vt100::Parser>,
    app_cursor: &AtomicBool,
    cols: u16,
    rows: u16,
) {
    let state = pane_seed_state(target).unwrap_or_default();
    // The alternate screen has no scrollback, so only the normal buffer pulls
    // history (`-S`); the pane keeps that history across re-arms.
    let seed_start = format!("-{SCROLLBACK_LINES}");
    let mut seed_args = vec!["capture-pane", "-t", target, "-p", "-e"];
    if !state.alt {
        seed_args.extend_from_slice(&["-S", &seed_start]);
    }
    let Ok(out) = crate::tmux::tmux_command().args(&seed_args).output() else {
        return;
    };
    let stream = assemble_seed_stream(&out.stdout, &state, rows);
    if let Ok(mut p) = parser.lock() {
        *p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        p.process(&stream);
        app_cursor.store(p.screen().application_cursor(), Ordering::Relaxed);
    }
}

/// Assemble the byte stream that seeds a fresh parser from a `capture-pane -e`
/// body plus the pane's queried [`PaneSeedState`]. Pure (no tmux), so the
/// coordinate mapping is unit-testable.
///
/// Order matters: the DEC private-mode SETs come first (the body carries none),
/// then the CRLF-normalised body (capture-pane joins rows with bare LF; the
/// parser needs CR to reset the column or each row staircases), then an
/// absolute CUP and the DECTCEM show/hide.
///
/// The body is fed faithfully, including the blank rows capture-pane pads out to
/// the full pane height, so the parser's visible screen is a pixel-for-pixel
/// replica of the pane. Only the single trailing line terminator is dropped:
/// with it, the final `\n` would push the whole screen up one row (the top row
/// scrolls into history) and misplace every cell. Because the visible screen is
/// faithful, the CUP is a plain 1-based `#{cursor_y}` / `#{cursor_x}`, which
/// addresses the visible screen regardless of how much scrollback sits behind
/// it (that is the coordinate space tmux reports the cursor in). Without this,
/// the parser's cursor lands after the last replayed glyph, bottom-right for a
/// full-screen app, until the first live chunk carries the app's own escapes
/// (issue #2902).
fn assemble_seed_stream(body: &[u8], state: &PaneSeedState, rows: u16) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(body.len() + 32);
    if state.alt {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    // Any-event tracking (1003) subsumes plain button tracking (1000); replay
    // whichever the app actually asked for so the grid's mode round-trips.
    if state.mouse_all {
        out.extend_from_slice(b"\x1b[?1003h");
    } else if state.mouse {
        out.extend_from_slice(b"\x1b[?1000h");
    }
    if state.mouse_sgr {
        out.extend_from_slice(b"\x1b[?1006h");
    }
    out.extend_from_slice(&lf_to_crlf(strip_trailing_row_terminator(body)));
    // 1-based CUP in visible-screen coordinates, clamped to the grid so a stale
    // query (the pane moved between the state read and this seed) can't push the
    // cursor off-screen; the first live chunk re-syncs it either way.
    let cy = state.cursor_y.min(rows.saturating_sub(1)) + 1;
    let cx = state.cursor_x + 1;
    out.extend_from_slice(format!("\x1b[{cy};{cx}H").as_bytes());
    out.extend_from_slice(if state.cursor_visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    out
}

/// Drop the single trailing line terminator (`\n` or `\r\n`) from a
/// `capture-pane` body. capture-pane terminates every row it emits, so the last
/// row carries a trailing newline that, if fed, scrolls the whole screen up one
/// row. The blank rows capture-pane pads the body with are kept: they hold the
/// visible screen at its true position so the seeded cursor's absolute row lands
/// on the right cell.
fn strip_trailing_row_terminator(raw: &[u8]) -> &[u8] {
    match raw.split_last() {
        Some((b'\n', rest)) => match rest.split_last() {
            Some((b'\r', rest2)) => rest2,
            _ => rest,
        },
        _ => raw,
    }
}

/// `pipe-pane -I` (input injection) landed in tmux 2.8, and a dead-pane write
/// crash was fixed in 3.4, so we require >= 3.4 before arming a channel. Older
/// tmux (or a `tmux -V` we can't parse) falls back to the capture path. Cached:
/// the server version doesn't change under a running aoe.
fn tmux_supports_pipe_pane_io() -> bool {
    static SUPPORTED: LazyLock<bool> = LazyLock::new(|| {
        let Ok(out) = crate::tmux::tmux_command().arg("-V").output() else {
            return false;
        };
        let v = String::from_utf8_lossy(&out.stdout);
        // e.g. "tmux 3.6", "tmux 3.4a", "tmux next-3.5".
        let digits: String = v
            .trim()
            .trim_start_matches(|c: char| !c.is_ascii_digit())
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut parts = digits.split('.');
        let major: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        let minor: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
        (major, minor) >= (3, 4)
    });
    *SUPPORTED
}

fn cursor_from_screen(screen: &vt100::Screen, rows: u16, cols: u16) -> PaneCursor {
    let (y, x) = screen.cursor_position();
    PaneCursor {
        x,
        y,
        visible: !screen.hide_cursor(),
        pane_height: rows,
        // Default; `sample` overrides this with the real scrollback depth.
        history_size: 0,
        pane_width: cols,
        alternate_on: screen.alternate_screen(),
        mouse_tracking: screen.mouse_protocol_mode() != vt100::MouseProtocolMode::None,
        mouse_sgr: screen.mouse_protocol_encoding() == vt100::MouseProtocolEncoding::Sgr,
        mouse_all: screen.mouse_protocol_mode() == vt100::MouseProtocolMode::AnyMotion,
        // Authoritative: the cursor is read straight from the owned grid, not
        // probed against a racing capture, so it is always trustworthy.
        position_reliable: true,
    }
}

/// Append the SGR parameters for one `vt100::Color` (foreground when `bg` is
/// false, background when true) to `params`.
fn push_color_params(params: &mut Vec<String>, color: vt100::Color, bg: bool) {
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(n) if n < 8 => {
            params.push((u16::from(n) + if bg { 40 } else { 30 }).to_string());
        }
        vt100::Color::Idx(n) if n < 16 => {
            params.push((u16::from(n - 8) + if bg { 100 } else { 90 }).to_string());
        }
        vt100::Color::Idx(n) => {
            params.push(if bg { "48".into() } else { "38".into() });
            params.push("5".into());
            params.push(n.to_string());
        }
        vt100::Color::Rgb(r, g, b) => {
            params.push(if bg { "48".into() } else { "38".into() });
            params.push("2".into());
            params.push(r.to_string());
            params.push(g.to_string());
            params.push(b.to_string());
        }
    }
}

/// Whether a cell carries any non-default styling (intensity, italic,
/// underline, inverse, or a non-default fg/bg colour). A blank-but-styled cell
/// is still visible: a background fill that runs to the edge of a row (a status
/// bar, a selection) has no glyph yet must be drawn.
fn cell_has_style(cell: &vt100::Cell) -> bool {
    cell.bold()
        || cell.dim()
        || cell.italic()
        || cell.underline()
        || cell.inverse()
        || !matches!(cell.fgcolor(), vt100::Color::Default)
        || !matches!(cell.bgcolor(), vt100::Color::Default)
}

/// The SGR escape that reproduces a cell's attributes, or an empty string for a
/// default (unstyled) cell.
fn cell_sgr(cell: &vt100::Cell) -> String {
    if !cell_has_style(cell) {
        return String::new();
    }
    let mut params: Vec<String> = Vec::new();
    if cell.bold() {
        params.push("1".into());
    }
    if cell.dim() {
        params.push("2".into());
    }
    if cell.italic() {
        params.push("3".into());
    }
    if cell.underline() {
        params.push("4".into());
    }
    if cell.inverse() {
        params.push("7".into());
    }
    push_color_params(&mut params, cell.fgcolor(), false);
    push_color_params(&mut params, cell.bgcolor(), true);
    if params.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", params.join(";"))
    }
}

/// Serialise one visible grid row to ANSI by walking its cells directly:
/// explicit SGR plus a literal character (or a space for a blank cell). vt100's
/// own `rows_formatted` encodes runs of blank cells as cursor-movement
/// (`ESC [ n C`) and erase-char (`ESC [ n X`) sequences. `ansi_to_tui`, the
/// downstream consumer that turns this string into a ratatui `Text`, ignores
/// cursor movement, so every gap of padding collapsed and aligned TUIs rendered
/// with their spaces stripped (#2433 regression). Emitting literal spaces keeps
/// the column layout intact while preserving colour and intensity.
fn row_to_ansi(screen: &vt100::Screen, row: u16, cols: u16) -> String {
    // Trim trailing *unstyled* blank cells, mirroring `capture-pane`'s
    // trailing-space trim, so a row never carries a full width of padding into
    // ratatui's wrapper. A trailing blank that carries styling (a background
    // fill running to the edge) is kept: it is drawn as a coloured space below,
    // exactly as a mid-row styled blank already is.
    let mut last = 0u16;
    for col in 0..cols {
        if screen
            .cell(row, col)
            .is_some_and(|cell| cell.has_contents() || cell_has_style(cell))
        {
            last = col + 1;
        }
    }

    let mut out = String::new();
    let mut cur_sgr: Option<String> = None;
    let mut col = 0u16;
    while col < last {
        let Some(cell) = screen.cell(row, col) else {
            out.push(' ');
            col += 1;
            continue;
        };
        // The trailing half of a wide character carries no contents of its own;
        // the lead cell already emitted the glyph that spans both columns.
        if cell.is_wide_continuation() {
            col += 1;
            continue;
        }
        let sgr = cell_sgr(cell);
        if cur_sgr.as_deref() != Some(sgr.as_str()) {
            // Reset first so a previous cell's attributes never bleed into this
            // one, then apply this cell's own (possibly empty) escape.
            out.push_str("\x1b[0m");
            out.push_str(&sgr);
            cur_sgr = Some(sgr);
        }
        if cell.has_contents() {
            out.push_str(cell.contents());
        } else {
            out.push(' ');
        }
        col += if cell.is_wide() { 2 } else { 1 };
    }
    out
}

/// Assemble the last `max_lines` rows of (scrollback + visible screen) as
/// per-row ANSI, and return that plus the full scrollback depth. vt100 only
/// formats the *visible* window, so we read it at successive scrollback offsets
/// (steps of one screen height) and stitch by absolute row index, then restore
/// the live-edge offset. Mirrors `capture-pane -S -<lines>`: history lines
/// first, the live screen as the last `rows` lines, `history` = total
/// scrollback.
fn grid_content(
    parser: &mut vt100::Parser,
    max_lines: usize,
    cols: u16,
    rows: u16,
) -> (String, usize) {
    let h = (rows as usize).max(1);
    let saved = parser.screen().scrollback();
    // Clamp to the maximum to discover how much scrollback actually exists.
    parser.screen_mut().set_scrollback(usize::MAX >> 4);
    let total_sb = parser.screen().scrollback();
    let total = total_sb + h;
    let want = max_lines.clamp(h.min(total), total);
    let target_low = total - want;

    // Absolute row index (0 = oldest scrollback, total-1 = bottom of screen).
    let mut buf: Vec<Option<String>> = vec![None; total];
    let mut offset = 0usize;
    loop {
        let real = offset.min(total_sb);
        parser.screen_mut().set_scrollback(real);
        let base = total_sb - real; // absolute index of this window's top row
        let screen = parser.screen();
        for r in 0..h {
            let g = base + r;
            if g < total {
                buf[g] = Some(row_to_ansi(screen, r as u16, cols));
            }
        }
        if real >= total_sb || base <= target_low {
            break;
        }
        offset += h;
    }
    parser.screen_mut().set_scrollback(saved);

    let mut content = String::new();
    for line in buf[target_low..total].iter() {
        if let Some(line) = line {
            content.push_str(line);
        }
        // Reset between rows so no SGR state bleeds across the newline.
        content.push_str("\x1b[0m\n");
    }
    (content, total_sb)
}

/// Shared state the reader thread owns for a channel's lifetime. A named
/// struct (rather than closure captures) so the reader loop is a plain
/// function tests can drive against a raw socket without arming a real
/// `pipe-pane`.
struct ReaderCtx {
    parser: Arc<Mutex<vt100::Parser>>,
    stop: Arc<AtomicBool>,
    seeded: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<UnixStream>>>,
    app_cursor: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    wakeup: Arc<Mutex<Option<ChangeWakeup>>>,
    /// Latest decoded OSC 52 clipboard write from the pane, awaiting a
    /// consumer (see [`VtChannel::take_clipboard`]). Single-slot: a newer
    /// copy overwrites an unconsumed older one, matching clipboard
    /// semantics (only the last copy matters).
    clipboard: Arc<Mutex<Option<String>>>,
    /// Chunk-arrival bookkeeping for the sample debounce (see the fields of the
    /// same name on `VtChannel`): a chunk counter, the last chunk's arrival
    /// (millis since `CHUNK_CLOCK`), and the gap between the two most recent
    /// chunks.
    chunk_seq: Arc<AtomicU64>,
    last_chunk_ms: Arc<AtomicU64>,
    prev_gap_ms: Arc<AtomicU64>,
    #[cfg(feature = "serve")]
    changed_tx: Arc<tokio::sync::watch::Sender<()>>,
}

/// The channel's reader loop: accept the forwarder's connection, publish the
/// writable half for input dispatch, then pump pane output into the vt100
/// grid, waking viewers on every change. Runs on its own thread; exits on
/// pipe EOF, socket error, or `stop`.
fn run_reader(listener: UnixListener, ctx: ReaderCtx) {
    let Ok((conn, _)) = listener.accept() else {
        return;
    };
    // Publish the writable half so input dispatch can reach the pane.
    if let Ok(w) = conn.try_clone() {
        *ctx.stream.lock().unwrap() = Some(w);
    }
    // The forwarder is connected: the channel is now the live
    // single-writer. `acquire` is blocked until this flips.
    ctx.alive.store(true, Ordering::Relaxed);
    let mut conn = conn;
    let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
    let mut buf = [0u8; 8192];
    let mut osc52 = Osc52Scanner::new();
    while !ctx.stop.load(Ordering::Relaxed) {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Hold stream bytes until the seed is applied so the
                // seed can't clobber newer state.
                while !ctx.seeded.load(Ordering::Relaxed) && !ctx.stop.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                // The vt100 parser below silently drops OSC 52, and in
                // live-send no tmux client is attached for `set-clipboard`
                // to forward to, so this tap is the ONLY path an agent's
                // copy has to the host clipboard (#2420).
                if let Some(text) = osc52.feed(&buf[..n]) {
                    if let Ok(mut guard) = ctx.clipboard.lock() {
                        *guard = Some(text);
                    }
                }
                if let Ok(mut p) = ctx.parser.lock() {
                    p.process(&buf[..n]);
                    ctx.app_cursor
                        .store(p.screen().application_cursor(), Ordering::Relaxed);
                }
                // Stamp this chunk's arrival so the capture worker can tell a
                // lone chunk (keystroke echo) from a back-to-back stream (a
                // multi-chunk repaint) and hold the sample until the stream
                // settles. Recorded before the wakeup so the woken worker reads
                // fresh timing.
                let seq = ctx.chunk_seq.fetch_add(1, Ordering::Relaxed);
                let now = chunk_now_ms();
                let prev = ctx.last_chunk_ms.swap(now, Ordering::Relaxed);
                ctx.prev_gap_ms.store(
                    if seq == 0 {
                        u64::MAX
                    } else {
                        now.saturating_sub(prev)
                    },
                    Ordering::Relaxed,
                );
                // Wake the in-process poller (the TUI capture worker) so the
                // just-landed output samples now, not after the remainder of
                // its poll interval. This is the echo-latency path.
                notify_change_wakeup(&ctx.wakeup);
                // Wake every viewer waiting on output. The watch
                // coalesces (a viewer that wasn't parked sees the
                // bumped version on its next wait), so a chunk that
                // lands between waits is not lost.
                #[cfg(feature = "serve")]
                ctx.changed_tx.send_modify(|_| {});
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    // Reader is exiting (pipe EOF / socket error / stop): the
    // forwarder is gone, so the channel is no longer the live
    // single-writer. Input dispatch and capture both fall back.
    ctx.alive.store(false, Ordering::Relaxed);
    // Wake parked viewers so they observe the death promptly
    // instead of waiting out their heartbeat sleep.
    notify_change_wakeup(&ctx.wakeup);
    #[cfg(feature = "serve")]
    ctx.changed_tx.send_modify(|_| {});
}

/// One shared pane channel: a vt100 grid fed by a `pipe-pane -IO` byte stream,
/// plus the writable half of the same socket for keystroke injection. Methods
/// take `&self` (interior mutability) so many viewers share one `Arc`.
pub(crate) struct VtChannel {
    /// tmux session name; the registry key.
    name: String,
    /// `name:^.0`, the pane target for tmux commands.
    target: String,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Writable half of the socket, `Some` once the forwarder connects. Shared
    /// with the reader thread, which fills it after `accept`.
    stream: Arc<Mutex<Option<UnixStream>>>,
    /// DECCKM snapshot, refreshed by the reader thread on each grid change.
    app_cursor: Arc<AtomicBool>,
    /// `true` while the forwarder is connected and the reader loop is running.
    /// Set once `accept` publishes the writable half; cleared when the reader
    /// exits (pipe EOF / socket error). `acquire` only returns after this goes
    /// true, so a live channel is the single-writer; once it clears, input and
    /// capture both fall back to the legacy tmux path instead of black-holing.
    alive: Arc<AtomicBool>,
    /// Bumped by the reader thread on each grid change (and on death). A watch
    /// (not a `Notify`) so EVERY viewer of this shared channel wakes on a
    /// change, not just one; `subscribe` hands each connection its own
    /// receiver. Server-only. The carried value is unused (it is the version
    /// bump that matters), so it is `()`.
    #[cfg(feature = "serve")]
    changed_tx: Arc<tokio::sync::watch::Sender<()>>,
    /// Slot for one in-process poller's wakeup (the TUI capture worker).
    /// The reader thread pokes it on each grid change and on death; last
    /// registration wins (one capture worker per process, so a slot rather
    /// than a list).
    wakeup: Arc<Mutex<Option<ChangeWakeup>>>,
    /// Latest decoded OSC 52 clipboard write from the pane, filled by the
    /// reader thread, drained by [`Self::take_clipboard`].
    clipboard: Arc<Mutex<Option<String>>>,
    /// Number of chunks the reader has parsed. `0` means none yet, so
    /// `chunk_timing` reports `None` and the caller leaves pacing untouched.
    chunk_seq: Arc<AtomicU64>,
    /// Arrival of the most recent chunk (millis since `CHUNK_CLOCK`), stamped
    /// by the reader thread on every chunk.
    last_chunk_ms: Arc<AtomicU64>,
    /// Interval between the two most recent chunks (millis). Large when the
    /// latest chunk followed a quiet gap (a lone keystroke echo); small during
    /// a back-to-back stream (a multi-chunk repaint). The sample debounce keys
    /// off this to tell the two apart.
    prev_gap_ms: Arc<AtomicU64>,
    /// Owner-only (0700) directory holding `sock_path`; removed on drop.
    sock_dir: PathBuf,
    sock_path: PathBuf,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    cols: AtomicU16,
    rows: AtomicU16,
    last_size_check: Mutex<Instant>,
}

impl VtChannel {
    /// The session's existing *live* channel, if any, WITHOUT arming a new
    /// one. The `[tmux] vt_live = false` web gate uses this so a connection
    /// opened after the toggle still joins a channel that other holders keep
    /// alive: while that channel lives it is the pane's single input writer,
    /// and a viewer that fell back to `send-keys` beside it would interleave
    /// two writers on the one pty input stream. Once the last holder drops,
    /// the channel dies and fallback becomes genuine. Server-only: the TUI
    /// worker owns its channel lifecycle and its input path already routes
    /// through the registry (`input_mode` / `try_send_input`).
    #[cfg(feature = "serve")]
    pub(crate) fn reuse(session: &str) -> Option<Arc<VtChannel>> {
        lookup(session).filter(|c| c.is_alive())
    }

    /// Get the shared channel for `session`, arming a new one if none is live.
    /// Returns `None` if tmux is too old or the pane is gone or any tmux/socket
    /// step fails; callers then use the legacy capture/send-keys path. The
    /// returned `Arc` keeps the channel alive; drop it to release this viewer's
    /// hold (the channel tears down when the last `Arc` drops).
    pub(crate) fn acquire(session: &str) -> Option<Arc<VtChannel>> {
        if let Some(ch) = lookup(session) {
            return Some(ch);
        }
        // Arm WITHOUT holding the registry lock: `arm` blocks up to ~500ms
        // waiting for the forwarder to connect, and the global lock is taken on
        // every `input_mode` / `try_send_input` for every session, so holding it
        // that long would stall all pane input. Re-check under the lock and
        // prefer a channel another thread armed in the meantime (ours drops).
        let ch = Arc::new(Self::arm(session)?);
        let mut reg = REGISTRY.lock().unwrap();
        if let Some(existing) = reg.get(session).and_then(Weak::upgrade) {
            return Some(existing);
        }
        reg.insert(session.to_string(), Arc::downgrade(&ch));
        Some(ch)
    }

    fn arm(name: &str) -> Option<Self> {
        if !tmux_supports_pipe_pane_io() {
            return None;
        }
        let target = format!("{name}:^.0");
        let (cols, rows) = pane_size(&target)?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
        let stop = Arc::new(AtomicBool::new(false));
        let seeded = Arc::new(AtomicBool::new(false));
        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(false));
        let clipboard: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        // Bumped by the reader thread on every grid change (and on death) so
        // a web viewer can render on output instead of polling on a cadence.
        // A watch so all viewers wake, not just one. Server-only; the TUI
        // repaints from its own draw loop. The initial receiver is dropped;
        // `subscribe` mints one per connection.
        #[cfg(feature = "serve")]
        let changed_tx = Arc::new(tokio::sync::watch::channel(()).0);

        // Bind the socket inside an owner-only (0700) directory so other users
        // on a shared host cannot connect to the pane channel and capture
        // keystrokes or spoof rendered output (mirrors the worker-dir
        // convention in `src/process/worker.rs`). On macOS/BSD the socket
        // file's own mode is ignored by `connect`, so the 0700 parent is the
        // real gate; the short per-channel path also stays well under the
        // macOS `sun_path` limit.
        let n = SOCK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let sock_dir = std::env::temp_dir().join(format!("aoe-vt-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        std::fs::create_dir_all(&sock_dir).ok()?;
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700)).ok()?;
        }
        let sock_path = sock_dir.join("s.sock");
        let listener = UnixListener::bind(&sock_path).ok()?;

        let wakeup: Arc<Mutex<Option<ChangeWakeup>>> = Arc::new(Mutex::new(None));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let last_chunk_ms = Arc::new(AtomicU64::new(0));
        let prev_gap_ms = Arc::new(AtomicU64::new(u64::MAX));
        let reader = {
            let ctx = ReaderCtx {
                parser: parser.clone(),
                stop: stop.clone(),
                seeded: seeded.clone(),
                stream: stream.clone(),
                app_cursor: app_cursor.clone(),
                alive: alive.clone(),
                wakeup: wakeup.clone(),
                clipboard: clipboard.clone(),
                chunk_seq: chunk_seq.clone(),
                last_chunk_ms: last_chunk_ms.clone(),
                prev_gap_ms: prev_gap_ms.clone(),
                #[cfg(feature = "serve")]
                changed_tx: changed_tx.clone(),
            };
            std::thread::spawn(move || run_reader(listener, ctx))
        };

        let exe = std::env::current_exe().ok()?;
        let pipe_cmd = format!(
            "{} __vt-pipe {}",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&sock_path.to_string_lossy())
        );
        let armed = crate::tmux::tmux_command()
            .args(["pipe-pane", "-IO", "-t", &target, &pipe_cmd])
            .output()
            .ok();
        if !armed.map(|o| o.status.success()).unwrap_or(false) {
            tracing::warn!(%target, "vt: tmux pipe-pane failed; falling back to capture");
            stop.store(true, Ordering::Relaxed);
            let _ = UnixStream::connect(&sock_path);
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&sock_dir);
            return None;
        }

        // Wait for the forwarder to actually connect before publishing the
        // channel. `input_mode` treats a live channel as the single-writer and
        // sends ALL pane input through the socket; if we returned during this
        // startup gap, early keystrokes would hit a not-yet-connected socket
        // and be dropped instead of falling back to `send-keys`. If the
        // forwarder never connects, tear down and fall back to capture.
        let connect_deadline = Instant::now() + Duration::from_millis(500);
        while !alive.load(Ordering::Relaxed) {
            if Instant::now() >= connect_deadline {
                tracing::warn!(%target, "vt: forwarder did not connect; falling back to capture");
                stop.store(true, Ordering::Relaxed);
                let _ = crate::tmux::tmux_command()
                    .args(["pipe-pane", "-t", &target])
                    .output();
                let _ = UnixStream::connect(&sock_path);
                let _ = reader.join();
                let _ = std::fs::remove_dir_all(&sock_dir);
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // Seed the current screen so an already-running agent shows up
        // immediately instead of starting blank (pipe-pane has no backlog).
        seed_parser(&target, &parser, &app_cursor, cols, rows);
        seeded.store(true, Ordering::Relaxed);
        tracing::info!(%target, cols, rows, "vt channel armed (pipe-pane -IO <-> vt100 grid)");

        Some(Self {
            name: name.to_string(),
            target,
            parser,
            stream,
            app_cursor,
            alive,
            #[cfg(feature = "serve")]
            changed_tx,
            wakeup,
            clipboard,
            chunk_seq,
            last_chunk_ms,
            prev_gap_ms,
            sock_dir,
            sock_path,
            stop,
            reader: Mutex::new(Some(reader)),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            last_size_check: Mutex::new(Instant::now()),
        })
    }

    /// Reconcile the parser size with the pane at most once a second (a
    /// `display-message` fork; rate-limited so it adds no periodic hitch). On a
    /// change, re-seed from `capture-pane` rather than just `set_size`: tmux
    /// reflows on resize but pipe-pane carries no reflow redraw, so a bare
    /// `set_size` would leave the grid diverged from tmux (see `seed_parser`).
    fn reconcile_size(&self) {
        let mut guard = self.last_size_check.lock().unwrap();
        if guard.elapsed() < Duration::from_secs(1) {
            return;
        }
        *guard = Instant::now();
        drop(guard);
        if let Some((c, r)) = pane_size(&self.target) {
            if (c, r)
                != (
                    self.cols.load(Ordering::Relaxed),
                    self.rows.load(Ordering::Relaxed),
                )
            {
                self.cols.store(c, Ordering::Relaxed);
                self.rows.store(r, Ordering::Relaxed);
                seed_parser(&self.target, &self.parser, &self.app_cursor, c, r);
            }
        }
    }

    /// Re-sync the in-process grid to the new pane size immediately. The
    /// size-owner calls this right after `resize-window` so the grid tracks the
    /// new geometry without waiting for the periodic `reconcile_size`. It
    /// re-seeds from `capture-pane` (tmux's authoritative reflowed state) rather
    /// than locally `set_size`-ing: tmux reflows on resize but pipe-pane carries
    /// no reflow redraw, so a bare `set_size` would leave the grid diverged from
    /// tmux - a duplicated prompt and a cursor stranded on the wrong row that no
    /// later output reconciles (see `seed_parser`). Server-only.
    #[cfg(feature = "serve")]
    pub(crate) fn set_grid_size(&self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 {
            return;
        }
        if (cols, rows)
            != (
                self.cols.load(Ordering::Relaxed),
                self.rows.load(Ordering::Relaxed),
            )
        {
            self.cols.store(cols, Ordering::Relaxed);
            self.rows.store(rows, Ordering::Relaxed);
            seed_parser(&self.target, &self.parser, &self.app_cursor, cols, rows);
        }
    }

    /// Serialise up to `max_lines` of (scrollback + screen) to per-row ANSI,
    /// plus the authoritative cursor (with `history_size` set to the full
    /// scrollback depth). `max_lines` mirrors the capture path's window: both
    /// the TUI scroll and the web's virtual scroll spacer need real history
    /// here, not just the visible screen.
    pub(crate) fn sample(&self, max_lines: usize) -> (String, Option<PaneCursor>) {
        self.reconcile_size();
        let cols = self.cols.load(Ordering::Relaxed);
        let rows = self.rows.load(Ordering::Relaxed);
        let mut p = match self.parser.lock() {
            Ok(p) => p,
            Err(_) => return (String::new(), None),
        };
        let (content, history) = grid_content(&mut p, max_lines, cols, rows);
        let mut cursor = cursor_from_screen(p.screen(), rows, cols);
        cursor.history_size = history as u32;
        (content, Some(cursor))
    }

    /// Whether the forwarder is connected and the reader loop is running. A
    /// channel that never connected, or whose pipe has since closed, reports
    /// `false` so input and capture fall back to the legacy tmux path instead
    /// of writing into a dead socket or sampling a frozen grid.
    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Take the newest OSC 52 clipboard write the pane has emitted since the
    /// last call, if any. Consuming and single-slot (a newer copy overwrites
    /// an unconsumed older one), so exactly one consumer should drain it: the
    /// TUI capture worker, which forwards it to the host clipboard. Queries
    /// and empty writes are filtered out at the scanner, so a taken value is
    /// always non-empty text.
    pub(crate) fn take_clipboard(&self) -> Option<String> {
        self.clipboard
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    /// Register the in-process poller wakeup this channel pokes on each grid
    /// change (and on death). The TUI capture worker hands over the same
    /// condvar pair its retarget/cadence nudges use, so pane output wakes it
    /// into an immediate sample instead of letting the echo sit out the
    /// remainder of a poll interval.
    pub(crate) fn set_change_wakeup(&self, wakeup: ChangeWakeup) {
        if let Ok(mut guard) = self.wakeup.lock() {
            *guard = Some(wakeup);
        }
    }

    /// Chunk-arrival timing for the capture worker's repaint-quiescence
    /// debounce: `(since_last_chunk_ms, prev_gap_ms)`. The first is how long ago
    /// the most recent chunk landed; the second is the interval between the two
    /// most recent chunks, large when the latest chunk followed a quiet gap (a
    /// lone keystroke echo) and small during a back-to-back stream (a
    /// multi-chunk repaint). `None` until the first chunk arrives, so the caller
    /// leaves frame pacing untouched.
    pub(crate) fn chunk_timing(&self) -> Option<(u64, u64)> {
        if self.chunk_seq.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let since_last = chunk_now_ms().saturating_sub(self.last_chunk_ms.load(Ordering::Relaxed));
        Some((since_last, self.prev_gap_ms.load(Ordering::Relaxed)))
    }

    /// A receiver that fires whenever the grid changes (output arrived) or the
    /// channel dies. Each connection holds its own, so every viewer of this
    /// shared channel wakes on a change; `changed()` also returns immediately
    /// if a bump happened since the last wait, so output between waits is never
    /// missed. Lets a viewer render on output instead of polling. Server-only.
    #[cfg(feature = "serve")]
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed_tx.subscribe()
    }

    fn write_input(&self, bytes: &[u8]) -> bool {
        use std::io::Write;
        let mut guard = self.stream.lock().unwrap();
        match guard.as_mut() {
            Some(stream) => stream.write_all(bytes).is_ok(),
            None => false,
        }
    }
}

impl Drop for VtChannel {
    fn drop(&mut self) {
        // Remove our registry entry, but only if it still points at us (a
        // concurrent re-arm under the same name must not be clobbered): our
        // weak no longer upgrades once we're dropping.
        {
            let mut reg = REGISTRY.lock().unwrap();
            if reg.get(&self.name).is_some_and(|w| w.upgrade().is_none()) {
                reg.remove(&self.name);
            }
        }
        self.stop.store(true, Ordering::Relaxed);
        let _ = crate::tmux::tmux_command()
            .args(["pipe-pane", "-t", &self.target])
            .output();
        // Unblock a reader still parked in accept().
        let _ = UnixStream::connect(&self.sock_path);
        if let Some(h) = self.reader.lock().unwrap().take() {
            let _ = h.join();
        }
        // Remove the whole per-channel 0700 dir (socket included).
        let _ = std::fs::remove_dir_all(&self.sock_dir);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grid_content_preserves_interior_padding() {
        // A TUI lays a row out by positioning the cursor, not by writing runs of
        // spaces: "A" at col 0, then jump the cursor to col 11 (`ESC[12G`) and
        // write "B". The 10 cells in between are *default* (never written), so
        // vt100's `rows_formatted` skips them with `ESC[10C` (cursor forward).
        // `ansi_to_tui` ignores cursor movement, so the gap collapsed to "AB"
        // and aligned UIs lost their spacing (#2433). The literal serialiser
        // must emit those columns as real spaces.
        let mut p = vt100::Parser::new(2, 20, 0);
        p.process(b"A\x1b[12GB");
        let (content, _) = grid_content(&mut p, 2, 20, 2);
        assert!(
            content.contains("A          B"),
            "interior padding collapsed:\n{content:?}"
        );
        // No cursor-forward escape may leak into preview content.
        assert!(
            !content.contains("\x1b[10C") && !content.contains("\x1b[C"),
            "cursor-forward escape leaked:\n{content:?}"
        );
    }

    #[test]
    fn lf_to_crlf_unstaircases_seed_rows() {
        // capture-pane joins rows with bare LF; fed raw, the vt100 parser
        // staircases each row off the previous one's end column. lf_to_crlf
        // must make every row start at column 0 (regression: an idle/parked
        // prompt whose seed never gets a live repaint rendered staircased,
        // putting the cursor on the wrong row).
        let raw = b"line-1\nline-2\nREADY> ";
        let mut staircased = vt100::Parser::new(6, 40, 0);
        staircased.process(raw);
        assert_eq!(
            staircased.screen().cell(1, 0).map(|c| c.contents()),
            Some(""),
            "control: bare LF should staircase (row 1 col 0 empty)"
        );

        let mut fixed = vt100::Parser::new(6, 40, 0);
        fixed.process(&lf_to_crlf(raw));
        assert_eq!(
            fixed.screen().cell(0, 0).map(|c| c.contents()),
            Some("l"),
            "row 0 starts at col 0"
        );
        assert_eq!(
            fixed.screen().cell(1, 0).map(|c| c.contents()),
            Some("l"),
            "row 1 must start at col 0, not staircase"
        );
        assert_eq!(
            fixed.screen().cell(2, 0).map(|c| c.contents()),
            Some("R"),
            "prompt row starts at col 0"
        );
    }

    #[test]
    fn lf_to_crlf_leaves_existing_crlf_alone() {
        assert_eq!(lf_to_crlf(b"a\r\nb"), b"a\r\nb");
        assert_eq!(lf_to_crlf(b"a\nb"), b"a\r\nb");
    }

    #[test]
    fn strip_trailing_row_terminator_drops_only_the_last_newline() {
        // Only the single terminating newline goes; the padded blank rows stay
        // so the visible screen keeps its true vertical position.
        assert_eq!(
            strip_trailing_row_terminator(b"line-1\nREADY> \n\n\n"),
            b"line-1\nREADY> \n\n"
        );
        // A CRLF terminator drops both bytes.
        assert_eq!(strip_trailing_row_terminator(b"a\r\nb\r\n"), b"a\r\nb");
        // No terminator: unchanged.
        assert_eq!(strip_trailing_row_terminator(b"READY>"), b"READY>");
        assert_eq!(strip_trailing_row_terminator(b""), b"");
    }

    #[test]
    fn seed_places_cursor_at_queried_position_not_end_of_content() {
        // Regression for #2902: a full-grid body (nothing to trim) plus a real
        // cursor position that differs from the end of the seeded content. The
        // seeded parser must land the cursor where tmux reported it, not
        // bottom-right where the last replayed glyph ended.
        let rows: u16 = 6;
        let cols: u16 = 20;
        // Six full rows, so the parser cursor would otherwise strand at the
        // bottom-right after the last glyph.
        let body = b"row0-full-content\nrow1-full-content\nrow2-full-content\nrow3-full-content\nrow4-full-content\nrow5-full-content\n";
        let state = PaneSeedState {
            cursor_x: 3,
            cursor_y: 1,
            cursor_visible: true,
            ..Default::default()
        };
        let mut p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        p.process(&assemble_seed_stream(body, &state, rows));

        assert_eq!(
            p.screen().cursor_position(),
            (1, 3),
            "cursor must sit at the queried (row 1, col 3), not end-of-content"
        );
        assert!(
            !p.screen().hide_cursor(),
            "cursor_flag=1 must show the cursor"
        );
        // The faithful body is still there: row 0 was not scrolled off by a
        // stray trailing newline.
        assert!(
            p.screen().contents().contains("row0-full-content"),
            "top row must survive (no over-scroll):\n{}",
            p.screen().contents()
        );
    }

    #[test]
    fn seed_hides_cursor_when_pane_hid_it() {
        // An app that parked its hardware cursor (DECTCEM off) reports
        // cursor_flag=0; the seed must hide the parser cursor to match, instead
        // of a fresh parser's visible-by-default caret (issue #2902).
        let state = PaneSeedState {
            cursor_x: 0,
            cursor_y: 0,
            cursor_visible: false,
            ..Default::default()
        };
        let mut p = vt100::Parser::new(4, 10, 0);
        p.process(&assemble_seed_stream(b"hi\n", &state, 4));
        assert!(
            p.screen().hide_cursor(),
            "cursor_flag=0 must hide the seeded cursor"
        );
    }

    #[test]
    fn seed_cursor_row_is_visible_screen_relative_with_scrollback() {
        // With scrollback seeded, the parser's visible screen is the LAST rows
        // of the grid, and history scrolls off the top. tmux reports the cursor
        // relative to the visible pane, so the CUP must land there regardless of
        // how deep the scrollback is.
        let rows: u16 = 4;
        let cols: u16 = 12;
        // Ten rows into a 4-row screen: six scroll into history, the last four
        // are the visible screen.
        let mut body = Vec::new();
        for i in 0..10 {
            body.extend_from_slice(format!("HL{i:02}\n").as_bytes());
        }
        let state = PaneSeedState {
            cursor_x: 2,
            cursor_y: 1,
            cursor_visible: true,
            ..Default::default()
        };
        let mut p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        p.process(&assemble_seed_stream(&body, &state, rows));
        assert_eq!(
            p.screen().cursor_position(),
            (1, 2),
            "cursor row is visible-screen-relative, not counted from the top of history"
        );
        // The visible screen shows the newest rows (HL06..HL09), oldest in
        // history.
        assert!(
            p.screen().contents().contains("HL09"),
            "newest row must be on the visible screen:\n{}",
            p.screen().contents()
        );
    }

    #[test]
    fn grid_content_preserves_color() {
        // SGR 31 (red fg) on "X" must round-trip as an SGR escape, not a bare
        // cursor move, so colour survives into the preview.
        let mut p = vt100::Parser::new(2, 20, 0);
        p.process(b"\x1b[31mX\x1b[0m");
        let (content, _) = grid_content(&mut p, 2, 20, 2);
        assert!(content.contains('X'), "glyph missing:\n{content:?}");
        assert!(
            content.contains("\x1b[31m") || content.contains("31m"),
            "red foreground lost:\n{content:?}"
        );
    }

    #[test]
    fn grid_content_keeps_trailing_styled_fill() {
        // "Hi" then a blue background erased to the end of the line (`ESC[K`
        // with a bg set): cols 2..10 carry a bgcolor but no glyph, like a status
        // bar or selection that runs to the right edge. They must survive as
        // coloured spaces, not be trimmed as if blank.
        let mut p = vt100::Parser::new(2, 10, 0);
        p.process(b"Hi\x1b[44m\x1b[K");
        let (content, _) = grid_content(&mut p, 2, 10, 2);
        let first = content.split('\n').next().unwrap_or("");
        assert!(
            first.contains("44m"),
            "trailing background fill dropped:\n{content:?}"
        );
        assert!(
            first.matches(' ').count() >= 8,
            "trailing fill should keep its eight cells as spaces:\n{content:?}"
        );
    }

    #[test]
    fn reader_pokes_registered_wakeup_on_grid_change() {
        use std::io::Write;

        // Drive run_reader against a raw socket pair (posing as the
        // pipe-pane forwarder), no tmux needed. This pins the echo-latency
        // wiring: pane output must poke the registered wakeup so the TUI
        // capture worker samples immediately instead of waiting out its poll
        // interval.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let alive = Arc::new(AtomicBool::new(false));
        let wakeup_slot: Arc<Mutex<Option<ChangeWakeup>>> = Arc::new(Mutex::new(None));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            // Seeded upfront: this test has no capture-pane seed to wait for.
            seeded: Arc::new(AtomicBool::new(true)),
            stream,
            app_cursor: Arc::new(AtomicBool::new(false)),
            alive: alive.clone(),
            wakeup: wakeup_slot.clone(),
            clipboard: Arc::new(Mutex::new(None)),
            chunk_seq: Arc::new(AtomicU64::new(0)),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            #[cfg(feature = "serve")]
            changed_tx: Arc::new(tokio::sync::watch::channel(()).0),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        let pair: ChangeWakeup = Arc::new((Mutex::new(()), Condvar::new()));
        *wakeup_slot.lock().unwrap() = Some(pair.clone());
        // Hold the parker's mutex BEFORE writing: the reader's notify takes
        // the same lock, so the wakeup cannot fire into the gap between this
        // write and the wait below (i.e. the wait result is deterministic).
        let guard = pair.0.lock().unwrap();
        conn.write_all(b"echo-marker").expect("write pane output");
        let (wake_guard, res) = pair
            .1
            .wait_timeout(guard, Duration::from_secs(5))
            .expect("wait");
        // Release the pair's mutex before joining: the reader's exit path
        // notifies the wakeup one last time (death), and that notify takes
        // this same lock. Holding it across `join` would deadlock.
        drop(wake_guard);
        assert!(
            !res.timed_out(),
            "a grid change must poke the registered wakeup"
        );
        // The wake postdates the parse (notify runs after the parser lock is
        // released), so the change is already in the grid.
        assert!(
            parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("echo-marker"),
            "pane bytes must land in the grid before the wakeup fires"
        );

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn osc52_scanner_extracts_bel_and_st_terminated_writes() {
        // "hello" = aGVsbG8=
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"before\x1b]52;c;aGVsbG8=\x07after"),
            Some("hello".to_string())
        );
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1b]52;c;aGVsbG8=\x1b\\"),
            Some("hello".to_string())
        );
        // Unpadded base64 ("hi" = aGk) must decode too.
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;aGk\x07"), Some("hi".to_string()));
        // Empty targets field (`52;;`) is the spec's shorthand for `c`.
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;;aGVsbG8=\x07"), Some("hello".to_string()));
    }

    #[test]
    fn osc52_scanner_survives_arbitrary_chunk_splits() {
        // pipe-pane delivers reads at arbitrary boundaries; a copy split at
        // every byte position must still extract.
        let seq = b"noise\x1b]52;c;aGVsbG8=\x07more";
        for split in 1..seq.len() {
            let mut s = Osc52Scanner::new();
            let first = s.feed(&seq[..split]);
            let second = s.feed(&seq[split..]);
            assert_eq!(
                first.or(second),
                Some("hello".to_string()),
                "split at byte {split} lost the copy"
            );
        }
    }

    #[test]
    fn osc52_scanner_skips_queries_and_empty_writes() {
        // A query asks the terminal to REPLY with the clipboard; forwarding
        // it as a write (empty pbcopy/xclip input) would CLEAR the host
        // clipboard. Same for an explicit empty payload.
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;?\x07"), None);
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;\x07"), None);
        // Undecodable payloads are dropped, not forwarded as garbage.
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;=====\x07"), None);
    }

    #[test]
    fn osc52_scanner_ignores_other_sequences_and_recovers() {
        let mut s = Osc52Scanner::new();
        // Title OSC, a CSI, an OSC 5-something that is not 52, then a real
        // copy: only the copy comes out, and prior garbage doesn't wedge
        // the state machine.
        assert_eq!(
            s.feed(b"\x1b]0;title\x07\x1b[31m\x1b]521;x\x07\x1b]52;c;aGVsbG8=\x07"),
            Some("hello".to_string())
        );
        // The last complete write in a chunk wins (clipboard semantics).
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;aGk=\x07"),
            Some("hi".to_string())
        );
    }

    #[test]
    fn osc52_scanner_unwraps_tmux_passthrough_wrapped_writes() {
        // An agent that wraps its OSC 52 in tmux DCS passthrough doubles the
        // inner ESCs: `ESC P tmux; ESC ESC ] 52 ... ESC \`. The scanner must
        // still find the copy (BEL-terminated inner form, as emitted by our
        // own clipboard.rs and by OpenCode).
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\"),
            Some("hello".to_string())
        );
        // ST-terminated inner form: the terminator arrives ESC-doubled.
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x1b\x1b\\\x1b\\"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn reader_publishes_osc52_clipboard_from_pane_stream() {
        use std::io::Write;

        // Drive run_reader against a raw socket (posing as the pipe-pane
        // forwarder): an OSC 52 write in the pane stream must land in the
        // channel's clipboard slot (#2420), while the surrounding bytes
        // still reach the grid.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(false));
        let clipboard: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            seeded: Arc::new(AtomicBool::new(true)),
            stream: Arc::new(Mutex::new(None)),
            app_cursor: Arc::new(AtomicBool::new(false)),
            alive: alive.clone(),
            wakeup: Arc::new(Mutex::new(None)),
            clipboard: clipboard.clone(),
            chunk_seq: Arc::new(AtomicU64::new(0)),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            #[cfg(feature = "serve")]
            changed_tx: Arc::new(tokio::sync::watch::channel(()).0),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");
        conn.write_all(b"visible\x1b]52;c;aGVsbG8=\x07")
            .expect("write pane output");

        let deadline = Instant::now() + Duration::from_secs(5);
        let copied = loop {
            if let Some(text) = clipboard.lock().unwrap().take() {
                break Some(text);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(copied.as_deref(), Some("hello"));
        assert!(
            parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("visible"),
            "non-clipboard bytes must still reach the grid"
        );

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn reader_chunk_timing_distinguishes_stream_from_lone_chunk() {
        use std::io::Write;

        // Drive run_reader against a raw socket (posing as the pipe-pane
        // forwarder), no tmux needed. The reader stamps each chunk's arrival;
        // `chunk_timing` feeds the capture worker's repaint-quiescence debounce,
        // which must tell a lone chunk (a keystroke echo, wide inter-chunk gap)
        // from a back-to-back stream (a multi-chunk repaint, small gap). Without
        // that distinction the worker samples half-repainted grids and the
        // paired terminal flashes (#2903).
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let last_chunk_ms = Arc::new(AtomicU64::new(0));
        let prev_gap_ms = Arc::new(AtomicU64::new(u64::MAX));
        let ctx = ReaderCtx {
            parser,
            stop: stop.clone(),
            // Seeded upfront: this test has no capture-pane seed to wait for.
            seeded: Arc::new(AtomicBool::new(true)),
            stream: Arc::new(Mutex::new(None)),
            app_cursor: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Mutex::new(None)),
            clipboard: Arc::new(Mutex::new(None)),
            chunk_seq: chunk_seq.clone(),
            last_chunk_ms: last_chunk_ms.clone(),
            prev_gap_ms: prev_gap_ms.clone(),
            #[cfg(feature = "serve")]
            changed_tx: Arc::new(tokio::sync::watch::channel(()).0),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        // Wait for the reader to finish processing the `n`th chunk. Writing the
        // next chunk only after the previous is consumed keeps them separate
        // reads (a unix stream is a byte stream, so two pending writes could
        // otherwise coalesce into one chunk).
        let wait_seq = |n: u64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while chunk_seq.load(Ordering::Relaxed) < n {
                assert!(
                    Instant::now() < deadline,
                    "reader did not process {n} chunks"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };

        // First chunk (the repaint's clear): no prior chunk, so the gap is
        // "infinite" and it classifies as lone, never streaming.
        conn.write_all(b"\x1b[2J").expect("write clear");
        wait_seq(1);
        assert_eq!(
            prev_gap_ms.load(Ordering::Relaxed),
            u64::MAX,
            "the first chunk after arming is lone, not streaming"
        );

        // Two back-to-back reprint chunks: the reader records a real, small gap.
        conn.write_all(b"partial").expect("write partial");
        wait_seq(2);
        conn.write_all(b" repaint").expect("write rest");
        wait_seq(3);
        let stream_gap = prev_gap_ms.load(Ordering::Relaxed);
        assert!(
            stream_gap < 20,
            "back-to-back chunks record a small gap, got {stream_gap}"
        );

        // A chunk after a quiet pause records a much wider gap, so it reads as a
        // lone chunk and samples immediately rather than debouncing.
        std::thread::sleep(Duration::from_millis(40));
        conn.write_all(b"!").expect("write lone");
        wait_seq(4);
        let quiet_gap = prev_gap_ms.load(Ordering::Relaxed);
        assert!(
            quiet_gap >= 20 && quiet_gap > stream_gap,
            "a chunk after a 40ms pause is lone (gap {quiet_gap}) vs streamed (gap {stream_gap})"
        );

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn grid_content_assembles_scrollback_and_screen() {
        // 4-row screen; 12 distinct lines means several rows scroll into
        // history. Markers are non-substrings of each other (LINE01 vs LINE12).
        let mut p = vt100::Parser::new(4, 20, 100);
        for i in 1..=12 {
            p.process(format!("LINE{i:02}\r\n").as_bytes());
        }

        // A wide window returns history + screen, history_size > 0.
        let (content, history) = grid_content(&mut p, 100, 20, 4);
        assert!(history > 0, "expected scrollback depth, got {history}");
        assert!(
            content.contains("LINE01"),
            "missing oldest line:\n{content}"
        );
        assert!(
            content.contains("LINE12"),
            "missing newest line:\n{content}"
        );

        // A screen-sized window returns only the live screen (no old history),
        // and the offset is restored to the live edge afterward.
        let (screen_only, _) = grid_content(&mut p, 4, 20, 4);
        assert!(
            !screen_only.contains("LINE01"),
            "screen-only window should not include scrollback:\n{screen_only}"
        );
        assert_eq!(p.screen().scrollback(), 0, "live-edge offset not restored");
    }
}
