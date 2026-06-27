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
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::tmux::PaneCursor;

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

/// Lines of scrollback the grid keeps, and how much history the seed pulls from
/// the pane. Matches tmux's default `history-limit` so a freshly armed channel
/// (e.g. after switching away from a session and back) has the pane's history
/// immediately, not just the visible screen.
const SCROLLBACK_LINES: usize = 2000;

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
    let out = Command::new("tmux")
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

/// Query the pane's terminal modes that the wheel-forward / scroll logic keys
/// off: `(alternate_on, mouse_tracking, mouse_sgr)`. Used once at arm to seed
/// the grid's modes, which the rendered-content seed can't carry.
fn pane_modes(target: &str) -> Option<(bool, bool, bool)> {
    let out = Command::new("tmux")
        .args([
            "display-message",
            "-p",
            "-t",
            target,
            "-F",
            "#{alternate_on} #{mouse_any_flag} #{mouse_sgr_flag}",
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
    let sgr = it.next().map(|f| f != "0").unwrap_or(false);
    Some((alt, mouse, sgr))
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
/// private-mode SETs; the pane's current modes are queried and replayed as a
/// prefix first, then the body is CRLF-translated (capture-pane uses bare LF;
/// the parser needs CR to reset the column or each row staircases).
fn seed_parser(
    target: &str,
    parser: &Mutex<vt100::Parser>,
    app_cursor: &AtomicBool,
    cols: u16,
    rows: u16,
) {
    let (alt, mouse, mouse_sgr) = pane_modes(target).unwrap_or((false, false, false));
    let mut prefix: Vec<u8> = Vec::new();
    if alt {
        prefix.extend_from_slice(b"\x1b[?1049h");
    }
    if mouse {
        prefix.extend_from_slice(b"\x1b[?1000h");
    }
    if mouse_sgr {
        prefix.extend_from_slice(b"\x1b[?1006h");
    }
    // The alternate screen has no scrollback, so only the normal buffer pulls
    // history (`-S`); the pane keeps that history across re-arms.
    let seed_start = format!("-{SCROLLBACK_LINES}");
    let mut seed_args = vec!["capture-pane", "-t", target, "-p", "-e"];
    if !alt {
        seed_args.extend_from_slice(&["-S", &seed_start]);
    }
    let Ok(out) = Command::new("tmux").args(&seed_args).output() else {
        return;
    };
    // Trim trailing blank rows: capture-pane pads the body out to the full pane
    // height, and feeding those empty rows would march the parser's cursor down
    // to the bottom, stranding it well below the app's actual last line. With
    // them gone the cursor naturally lands right after the final glyph (the
    // prompt), matching the app, and the position is in the grid's own
    // coordinates so it can't drift a row off a separately-queried cursor.
    let body = trim_trailing_blank_rows(&out.stdout);
    if let Ok(mut p) = parser.lock() {
        *p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        if !prefix.is_empty() {
            p.process(&prefix);
        }
        p.process(&lf_to_crlf(body));
        app_cursor.store(p.screen().application_cursor(), Ordering::Relaxed);
    }
}

/// Drop trailing whitespace (blank rows, trailing newlines, trailing spaces)
/// from a `capture-pane` body, so the seeded cursor ends right after the last
/// real glyph instead of marching down through the pane's empty rows. The
/// column it lands on doesn't matter to viewers (only the row is rendered as a
/// cursor cell); a fullscreen app fills the screen so there is nothing to trim.
fn trim_trailing_blank_rows(raw: &[u8]) -> &[u8] {
    let mut end = raw.len();
    while end > 0 && matches!(raw[end - 1], b' ' | b'\n' | b'\r' | b'\t') {
        end -= 1;
    }
    &raw[..end]
}

/// `pipe-pane -I` (input injection) landed in tmux 2.8, and a dead-pane write
/// crash was fixed in 3.4, so we require >= 3.4 before arming a channel. Older
/// tmux (or a `tmux -V` we can't parse) falls back to the capture path. Cached:
/// the server version doesn't change under a running aoe.
fn tmux_supports_pipe_pane_io() -> bool {
    static SUPPORTED: LazyLock<bool> = LazyLock::new(|| {
        let Ok(out) = Command::new("tmux").arg("-V").output() else {
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

        let reader = {
            let parser = parser.clone();
            let stop = stop.clone();
            let seeded = seeded.clone();
            let stream = stream.clone();
            let app_cursor = app_cursor.clone();
            let alive = alive.clone();
            #[cfg(feature = "serve")]
            let changed_tx = changed_tx.clone();
            std::thread::spawn(move || {
                let Ok((conn, _)) = listener.accept() else {
                    return;
                };
                // Publish the writable half so input dispatch can reach the pane.
                if let Ok(w) = conn.try_clone() {
                    *stream.lock().unwrap() = Some(w);
                }
                // The forwarder is connected: the channel is now the live
                // single-writer. `acquire` is blocked until this flips.
                alive.store(true, Ordering::Relaxed);
                let mut conn = conn;
                let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
                let mut buf = [0u8; 8192];
                while !stop.load(Ordering::Relaxed) {
                    match conn.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            // Hold stream bytes until the seed is applied so the
                            // seed can't clobber newer state.
                            while !seeded.load(Ordering::Relaxed) && !stop.load(Ordering::Relaxed) {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            if let Ok(mut p) = parser.lock() {
                                p.process(&buf[..n]);
                                app_cursor
                                    .store(p.screen().application_cursor(), Ordering::Relaxed);
                            }
                            // Wake every viewer waiting on output. The watch
                            // coalesces (a viewer that wasn't parked sees the
                            // bumped version on its next wait), so a chunk that
                            // lands between waits is not lost.
                            #[cfg(feature = "serve")]
                            changed_tx.send_modify(|_| {});
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
                alive.store(false, Ordering::Relaxed);
                // Wake parked viewers so they observe the death promptly
                // instead of waiting out their heartbeat sleep.
                #[cfg(feature = "serve")]
                changed_tx.send_modify(|_| {});
            })
        };

        let exe = std::env::current_exe().ok()?;
        let pipe_cmd = format!(
            "{} __vt-pipe {}",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&sock_path.to_string_lossy())
        );
        let armed = Command::new("tmux")
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
                let _ = Command::new("tmux")
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
        let _ = Command::new("tmux")
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
    fn trim_trailing_blank_rows_strips_pane_padding() {
        // capture-pane pads the body to the full pane height; without trimming,
        // the seeded cursor would march down those empty rows and land far
        // below the prompt (regression: cursor one row below the input box).
        assert_eq!(
            trim_trailing_blank_rows(b"line-1\nREADY> \n\n\n"),
            b"line-1\nREADY>"
        );
        assert_eq!(trim_trailing_blank_rows(b"READY>"), b"READY>");
        // Interior blanks are preserved; only the trailing run is trimmed.
        assert_eq!(trim_trailing_blank_rows(b"a\n\nb\n  \n"), b"a\n\nb");
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
