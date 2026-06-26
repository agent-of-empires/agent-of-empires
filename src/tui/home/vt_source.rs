//! Experimental in-process VT rendering source for the live preview, gated by
//! the `AOE_VT_LIVE` env var (see `LiveCaptureWorker`).
//!
//! Instead of forking `tmux capture-pane` every cadence tick and re-parsing a
//! lossy text snapshot through `ansi-to-tui`, this arms `tmux pipe-pane` once to
//! stream the pane's RAW output bytes (escape sequences included) into an
//! in-process [`vt100::Parser`] that owns a real grid (alt-screen buffer,
//! cursor, mouse/DEC modes). The pipe target is `aoe __vt-pipe <socket>`, a
//! tiny forwarder that copies stdin to a unix socket this process listens on
//! (mirrors the ACP runner's socket plumbing; avoids a `cat` buffering relay).
//!
//! `sample()` serialises the visible grid back to per-row ANSI via
//! [`vt100::Screen::rows_formatted`], so the existing preview render path
//! (`parse_output_text` -> `ansi-to-tui` -> `Paragraph`, plus the cursor
//! overlay) consumes it unchanged. Only the transport changes.
//!
//! Scope (spike): visible screen only, no scrollback exposure, render-only
//! (input still flows through `LiveSendWorker`). Unix-only; the whole module is
//! `#[cfg(unix)]`.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use crate::tmux::PaneCursor;

/// `aoe __vt-pipe <socket>`: the bidirectional pipe-pane forwarder for
/// `tmux pipe-pane -IO`. tmux connects the pane's OUTPUT to this process's
/// stdin and the pane's INPUT to its stdout, so:
///   - stdin (pane output) -> socket  (the TUI reads it into a vt100 grid)
///   - socket -> stdout (pane input)  (the TUI writes keystrokes, no fork)
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

/// One pane's input channel: the writable half of its socket (`Some` once the
/// forwarder connects) plus a snapshot of the keyboard modes the encoder needs.
/// `app_cursor` (DECCKM) is refreshed by the output reader thread each time the
/// grid changes, so the input path can encode arrows correctly without locking
/// the parser on every keystroke.
pub(in crate::tui) struct InputChannel {
    stream: Mutex<Option<UnixStream>>,
    app_cursor: AtomicBool,
}

/// Per-session input channels keyed by tmux session name. The input dispatch
/// path (`live_send::dispatch_via_fork`) consults this to write keystroke bytes
/// straight to the pane instead of forking `tmux send-keys`.
static VT_INPUT_SINKS: LazyLock<Mutex<HashMap<String, Arc<InputChannel>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn channel_for(session: &str) -> Option<Arc<InputChannel>> {
    VT_INPUT_SINKS.lock().unwrap().get(session).cloned()
}

/// If `session` has an armed input channel, return its current cursor-key mode
/// (DECCKM): `Some(true)` = application cursor keys (`ESC O A`), `Some(false)` =
/// normal (`ESC [ A`). `None` means no channel is armed, so the caller must use
/// the `send-keys` fork path. Presence of `Some` is the single-writer signal:
/// while armed, ALL pane input must go through `try_send_input`.
pub(in crate::tui) fn input_mode(session: &str) -> Option<bool> {
    channel_for(session).map(|c| c.app_cursor.load(Ordering::Relaxed))
}

/// Deliver `bytes` to `session`'s pane via its persistent input channel.
/// Returns `true` if written, `false` if the channel is gone or the forwarder
/// has not connected yet.
pub(in crate::tui) fn try_send_input(session: &str, bytes: &[u8]) -> bool {
    use std::io::Write;
    let Some(channel) = channel_for(session) else {
        return false;
    };
    let mut guard = channel.stream.lock().unwrap();
    match guard.as_mut() {
        Some(stream) => stream.write_all(bytes).is_ok(),
        None => false,
    }
}

static SOCK_COUNTER: AtomicU64 = AtomicU64::new(0);

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

/// `pipe-pane -I` (input injection) landed in tmux 2.8, and a dead-pane write
/// crash was fixed in 3.4, so we require >= 3.4 before arming a VT channel.
/// Older tmux (or a `tmux -V` we can't parse) falls back to the capture path.
/// Cached: the server version doesn't change under a running aoe.
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
        // Scrollback exposure is out of scope for the spike; the preview's
        // own scroll math tolerates 0 here.
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

/// One armed pane: an in-process vt100 grid fed by a `pipe-pane -IO` byte
/// stream, plus the writable half of the same socket for keystroke injection.
pub(in crate::tui) struct VtSource {
    /// tmux session name; the registry key for this pane's input channel.
    name: String,
    /// `name:^.0`, the pane target for tmux commands.
    target: String,
    parser: Arc<Mutex<vt100::Parser>>,
    sock_path: PathBuf,
    stop: Arc<AtomicBool>,
    reader: Option<std::thread::JoinHandle<()>>,
    cols: u16,
    rows: u16,
    last_size_check: std::time::Instant,
}

impl VtSource {
    /// Arm `pipe-pane -IO` for `name`, seed the current screen once, start
    /// streaming output into a fresh parser, and register the socket's writable
    /// half as the pane's input channel. Returns `None` if tmux is too old or
    /// the pane is gone or any tmux/socket step fails; the worker then falls
    /// back to the legacy capture/send-keys path for this pane.
    pub(in crate::tui) fn arm(name: &str) -> Option<Self> {
        if !tmux_supports_pipe_pane_io() {
            return None;
        }
        let target = format!("{name}:^.0");
        let (cols, rows) = pane_size(&target)?;
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 1000)));
        let stop = Arc::new(AtomicBool::new(false));
        let seeded = Arc::new(AtomicBool::new(false));

        // Input channel: registered now (socket empty, filled once the
        // forwarder connects). While registered, the dispatch path routes ALL
        // pane input here (single-writer); the socket only carries bytes once
        // connected, so keys before then are dropped rather than forked.
        let channel = Arc::new(InputChannel {
            stream: Mutex::new(None),
            app_cursor: AtomicBool::new(false),
        });
        VT_INPUT_SINKS
            .lock()
            .unwrap()
            .insert(name.to_string(), channel.clone());

        let n = SOCK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let sock_path =
            std::env::temp_dir().join(format!("aoe-vt-{}-{n}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).ok()?;

        let reader = {
            let parser = parser.clone();
            let stop = stop.clone();
            let seeded = seeded.clone();
            let channel = channel.clone();
            std::thread::spawn(move || {
                let Ok((conn, _)) = listener.accept() else {
                    return;
                };
                // Publish the writable half so input dispatch can reach the pane.
                if let Ok(w) = conn.try_clone() {
                    *channel.stream.lock().unwrap() = Some(w);
                }
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
                                // Refresh the cursor-key mode for the input
                                // encoder (cheap; the app toggles DECCKM via its
                                // output, which only this thread sees).
                                channel
                                    .app_cursor
                                    .store(p.screen().application_cursor(), Ordering::Relaxed);
                            }
                        }
                        Err(ref e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                || e.kind() == std::io::ErrorKind::TimedOut => {}
                        Err(_) => break,
                    }
                }
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
        let armed_ok = armed.map(|o| o.status.success()).unwrap_or(false);
        if !armed_ok {
            tracing::warn!(%target, "vt_source: tmux pipe-pane failed; preview will be blank");
            VT_INPUT_SINKS.lock().unwrap().remove(name);
            stop.store(true, Ordering::Relaxed);
            let _ = UnixStream::connect(&sock_path);
            let _ = reader.join();
            let _ = std::fs::remove_file(&sock_path);
            return None;
        }

        // Seed the visible screen so an already-running agent shows up
        // immediately instead of starting blank.
        if let Ok(out) = Command::new("tmux")
            .args(["capture-pane", "-t", &target, "-p", "-e"])
            .output()
        {
            if let Ok(mut p) = parser.lock() {
                p.process(&out.stdout);
                channel
                    .app_cursor
                    .store(p.screen().application_cursor(), Ordering::Relaxed);
            }
        }
        seeded.store(true, Ordering::Relaxed);
        tracing::info!(%target, cols, rows, "vt_source armed (pipe-pane -IO <-> vt100 grid + input)");

        Some(Self {
            name: name.to_string(),
            target,
            parser,
            sock_path,
            stop,
            reader: Some(reader),
            cols,
            rows,
            last_size_check: std::time::Instant::now(),
        })
    }

    /// Serialise the current grid to per-row ANSI for the preview render path,
    /// plus the authoritative cursor. Reconciles the parser size with the pane
    /// at most once a second (a `display-message` fork; rate-limited so it does
    /// not add a periodic hitch to the otherwise fork-free sample path).
    pub(in crate::tui) fn sample(&mut self) -> (String, Option<PaneCursor>) {
        if self.last_size_check.elapsed() >= Duration::from_secs(1) {
            self.last_size_check = std::time::Instant::now();
            if let Some((c, r)) = pane_size(&self.target) {
                if (c, r) != (self.cols, self.rows) {
                    self.cols = c;
                    self.rows = r;
                    if let Ok(mut p) = self.parser.lock() {
                        p.screen_mut().set_size(r, c);
                    }
                }
            }
        }

        let p = match self.parser.lock() {
            Ok(p) => p,
            Err(_) => return (String::new(), None),
        };
        let screen = p.screen();
        let mut content = String::new();
        for row in screen.rows_formatted(0, self.cols) {
            content.push_str(&String::from_utf8_lossy(&row));
            // Reset between rows so no SGR state bleeds across the newline.
            content.push_str("\x1b[0m\n");
        }
        let cursor = cursor_from_screen(screen, self.rows, self.cols);
        (content, Some(cursor))
    }
}

impl Drop for VtSource {
    fn drop(&mut self) {
        // Retire the input channel first so no keystroke races a dying socket.
        VT_INPUT_SINKS.lock().unwrap().remove(&self.name);
        self.stop.store(true, Ordering::Relaxed);
        // Disable the pipe so tmux stops the forwarder.
        let _ = Command::new("tmux")
            .args(["pipe-pane", "-t", &self.target])
            .output();
        // Unblock a reader still parked in accept().
        let _ = UnixStream::connect(&self.sock_path);
        if let Some(h) = self.reader.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.sock_path);
    }
}
