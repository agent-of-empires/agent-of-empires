//! Shared in-process VT channel: a `tmux pipe-pane` stream feeds an in-process
//! [`vt100::Parser`], and where tmux allows it the same unix socket carries
//! keystrokes back. tmux still owns the pane. One refcounted [`VtChannel`] per
//! session, torn down when the last `Arc` drops.

use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use base64::Engine;

use crate::tmux::osc8::{Osc8Scanner, PaneLink};
use crate::tmux::PaneCursor;

/// Bounds the OSC 52 payload accumulator against a malformed stream.
const OSC52_MAX_PAYLOAD: usize = 2 * 1024 * 1024;

#[derive(Clone, Copy, PartialEq)]
enum Osc52State {
    Idle,
    Esc,
    OscStart,
    Five,
    Two,
    /// Inside the selection-target params, up to the `;` opening the payload.
    Params,
    Payload,
    /// `ESC` inside the payload: an ST, or a passthrough-doubled `ESC ESC \`.
    PayloadEsc,
}

/// Per-byte OSC 52 clipboard-write extractor whose state survives chunk
/// boundaries. Queries and empty payloads are skipped: forwarding an empty
/// write would clear the host clipboard.
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

    /// Returns the last complete non-empty clipboard write in the chunk.
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
                // tmux passthrough doubles inner ESCs, so ST arrives as `ESC ESC \`.
                (PayloadEsc, 0x1b) => PayloadEsc,
                (PayloadEsc, _) => Idle,
                (Esc | OscStart | Five | Two, 0x1b) => Esc,
                (Esc | OscStart | Five | Two, _) => Idle,
            };
        }
        found
    }

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

fn is_payload_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'=' | b'?')
}

/// Longest a DEC 2026 bracket suppresses viewer wakeups, so death detection
/// and heartbeats keep running.
const SYNC_HOLD_MAX_MS: u64 = 200;
/// Longest the sampler serves the last complete frame over a mid-bracket grid;
/// past this the app is stuck and its partial screen is all there is.
const SYNC_BRACKET_ABANDON_MS: u64 = 2_000;

#[derive(Clone, Copy, PartialEq)]
enum SyncState {
    Idle,
    Esc,
    Csi,
    Params,
}

/// Per-byte detector for `CSI ? <params> h|l` with mode 2026 (synchronized
/// output). Params longer than 32 bytes are abandoned, so it never grows.
struct SyncOutputScanner {
    state: SyncState,
    params: Vec<u8>,
}

impl SyncOutputScanner {
    fn new() -> Self {
        Self {
            state: SyncState::Idle,
            params: Vec::new(),
        }
    }

    /// Append the chunk's 2026 transitions in order (`true` = opened); one read
    /// can close one repaint and open the next.
    fn feed(&mut self, chunk: &[u8], out: &mut Vec<bool>) {
        use SyncState::*;
        for &b in chunk {
            self.state = match (self.state, b) {
                (Idle, 0x1b) => Esc,
                (Idle, _) => Idle,
                (Esc, b'[') => Csi,
                (Csi, b'?') => {
                    self.params.clear();
                    Params
                }
                (Params, b'0'..=b'9' | b';') if self.params.len() < 32 => {
                    self.params.push(b);
                    Params
                }
                (Params, b'h' | b'l') => {
                    if self.params.split(|&c| c == b';').any(|p| p == b"2026") {
                        out.push(b == b'h');
                    }
                    Idle
                }
                (Esc | Csi | Params, 0x1b) => Esc,
                (Esc | Csi | Params, _) => Idle,
            };
        }
    }
}

/// How a chunk's 2026 transitions move the hold around parsing it: opening is
/// raised before the bytes land, closing after. A close-then-open restarts the
/// bracket's timestamp but not the incomplete run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct SyncHoldPlan {
    open: bool,
    /// A close precedes that opener, so the new bracket needs a fresh timestamp.
    restart: bool,
    close: bool,
}

impl SyncHoldPlan {
    fn from_events(events: &[bool]) -> Self {
        let last_open = events.iter().rposition(|&open| open);
        Self {
            open: last_open.is_some(),
            restart: last_open.is_some_and(|i| events[..i].contains(&false)),
            close: events.last() == Some(&false),
        }
    }

    fn begin(&self, signals: &ViewerSignals, now: impl FnOnce() -> u64) {
        if self.restart {
            signals.restart_hold(now());
        } else if self.open {
            signals.begin_hold(now());
        }
    }

    /// Under the parser lock, so a sampler never sees the release first.
    fn end(&self, signals: &ViewerSignals) {
        if self.close {
            signals.end_hold();
        }
    }
}

/// Reader-thread signals for viewers outside the process loop: a change watch,
/// a non-consuming clipboard slot with a sequence, and the 2026 hold.
pub(crate) struct ViewerSignals {
    changed_tx: tokio::sync::watch::Sender<()>,
    clipboard_latest: Mutex<Option<String>>,
    clipboard_seq: AtomicU64,
    /// `CHUNK_CLOCK` millis when the current bracket opened; 0 when none.
    sync_hold_since_ms: AtomicU64,
    /// `CHUNK_CLOCK` millis when the grid stopped holding a whole frame; 0 while it
    /// holds one. Not restarted by the next bracket, or an app whose repaints
    /// straddle every read could freeze the view forever.
    incomplete_since_ms: AtomicU64,
}

impl ViewerSignals {
    fn new() -> Self {
        Self {
            changed_tx: tokio::sync::watch::channel(()).0,
            clipboard_latest: Mutex::new(None),
            clipboard_seq: AtomicU64::new(0),
            sync_hold_since_ms: AtomicU64::new(0),
            incomplete_since_ms: AtomicU64::new(0),
        }
    }

    fn bump_changed(&self) {
        self.changed_tx.send_modify(|_| {});
    }

    fn publish_clipboard(&self, text: &str) {
        if let Ok(mut slot) = self.clipboard_latest.lock() {
            *slot = Some(text.to_string());
        }
        self.clipboard_seq.fetch_add(1, Ordering::Release);
    }

    fn begin_hold(&self, now: u64) {
        let now = now.max(1);
        if self.sync_hold_since_ms.load(Ordering::Relaxed) == 0 {
            self.sync_hold_since_ms.store(now, Ordering::Relaxed);
        }
        if self.incomplete_since_ms.load(Ordering::Relaxed) == 0 {
            self.incomplete_since_ms.store(now, Ordering::Relaxed);
        }
    }

    fn end_hold(&self) {
        self.sync_hold_since_ms.store(0, Ordering::Relaxed);
        self.incomplete_since_ms.store(0, Ordering::Relaxed);
    }

    /// Restart the bracket when its opener shares a read with the previous close,
    /// in one store. The incomplete run keeps running: that closed frame was never
    /// in the grid on its own.
    fn restart_hold(&self, now: u64) {
        let now = now.max(1);
        self.sync_hold_since_ms.store(now, Ordering::Relaxed);
        if self.incomplete_since_ms.load(Ordering::Relaxed) == 0 {
            self.incomplete_since_ms.store(now, Ordering::Relaxed);
        }
    }

    /// A 2026 bracket is open within [`SYNC_HOLD_MAX_MS`] (and the grid is still
    /// incomplete). Gates wakeups and publication.
    pub(crate) fn hold_active(&self) -> bool {
        self.hold_active_at(chunk_now_ms())
    }

    fn hold_active_at(&self, now: u64) -> bool {
        open_within(
            self.sync_hold_since_ms.load(Ordering::Relaxed),
            now,
            SYNC_HOLD_MAX_MS,
        ) && self.incomplete_within(now)
    }

    /// The grid holds an unfinished frame, bounded by [`SYNC_BRACKET_ABANDON_MS`]
    /// from the start of the run so the worst case is tearing, never freezing.
    fn incomplete_within(&self, now_ms: u64) -> bool {
        open_within(
            self.incomplete_since_ms.load(Ordering::Relaxed),
            now_ms,
            SYNC_BRACKET_ABANDON_MS,
        )
    }
}

/// Whether a hold stamped at `since` (0 = none) is still inside `window_ms`.
fn open_within(since: u64, now_ms: u64, window_ms: u64) -> bool {
    since != 0 && now_ms.saturating_sub(since) < window_ms
}

/// Drain-barrier frames on the forwarder's control socket: a probe carries a
/// generation, and the ACK is sent only between forwarding iterations with the
/// stdin queue empty.
const DRAIN_PROBE: u8 = b'Q';
const DRAIN_ACK: u8 = b'D';
const DRAIN_GENERATION_BYTES: usize = std::mem::size_of::<u64>();
const DRAIN_FRAME_BYTES: usize = 1 + DRAIN_GENERATION_BYTES;

fn drain_frame(kind: u8, generation: u64) -> [u8; DRAIN_FRAME_BYTES] {
    let mut frame = [0; DRAIN_FRAME_BYTES];
    frame[0] = kind;
    frame[1..].copy_from_slice(&generation.to_le_bytes());
    frame
}

fn read_drain_frame(mut stream: impl std::io::Read) -> std::io::Result<(u8, u64)> {
    let mut frame = [0; DRAIN_FRAME_BYTES];
    stream.read_exact(&mut frame)?;
    Ok((
        frame[0],
        u64::from_le_bytes(frame[1..].try_into().expect("drain frame generation")),
    ))
}

/// One unbuffered `read(2)`, retried on `EINTR`, so pane bytes are either still
/// visible to `FIONREAD` or in the caller's buffer.
fn read_raw(fd: std::os::fd::RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if n >= 0 {
            return Ok(n as usize);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// `aoe __vt-pipe <socket>`, the `pipe-pane` forwarder: stdin (pane output) to
/// the socket, and the socket to stdout (pane input, when armed `-IO`).
/// Unbuffered so a keystroke is never stalled behind a stdio buffer.
pub(crate) fn run_pipe(socket: &str) -> std::io::Result<()> {
    use std::io::Write;
    let sock_r = UnixStream::connect(socket)?;
    let sock_w = sock_r.try_clone()?;
    // Drain-barrier control socket next to the data socket. Read-only OSC 52
    // observers bind none; forwarding proceeds regardless.
    let ctl = socket
        .rsplit_once('/')
        .map(|(dir, _)| format!("{dir}/c.sock"))
        .and_then(|p| UnixStream::connect(p).ok());

    let pump_out = std::thread::spawn(move || {
        pump_pane_output(libc::STDIN_FILENO, &sock_w, ctl.as_ref());
        let _ = sock_w.shutdown(std::net::Shutdown::Write);
    });

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

/// Pump pane output into `sock_w`, answering drain probes on `ctl` only at the
/// top of the loop, after forwarding the whole stdin backlog: tmux queues pane
/// output to `pipe-pane` before parsing it, so bytes still inside this process
/// would otherwise escape the snapshot fence.
fn pump_pane_output(stdin_fd: std::os::fd::RawFd, sock_w: &UnixStream, ctl: Option<&UnixStream>) {
    pump_pane_output_with_hook(stdin_fd, sock_w, ctl, &mut || {});
}

fn pump_pane_output_with_hook<F: FnMut()>(
    stdin_fd: std::os::fd::RawFd,
    mut sock_w: &UnixStream,
    ctl: Option<&UnixStream>,
    after_read: &mut F,
) {
    use std::io::Write;
    let mut buf = [0u8; 8192];
    let mut ctl_open = ctl.is_some();
    loop {
        let mut fds = [
            libc::pollfd {
                fd: stdin_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: match (ctl, ctl_open) {
                    (Some(c), true) => c.as_raw_fd(),
                    _ => -1,
                },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if ready == -1 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if fds[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            match read_raw(stdin_fd, &mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    after_read();
                    if sock_w.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        if ctl_open && fds[1].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let Some(mut ctl) = ctl else {
                unreachable!("ctl_open implies ctl")
            };
            match read_drain_frame(ctl) {
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => ctl_open = false,
                Ok((DRAIN_PROBE, generation)) => {
                    // Forward the backlog first so the ACK covers it; if it cannot be proven
                    // empty, stay silent and let the channel time out into Busy.
                    let mut ack = true;
                    loop {
                        let mut pending: libc::c_int = 0;
                        if unsafe { libc::ioctl(stdin_fd, libc::FIONREAD, &mut pending) } != 0 {
                            ack = false;
                            break;
                        }
                        if pending <= 0 {
                            break;
                        }
                        match read_raw(stdin_fd, &mut buf) {
                            Ok(0) => break,
                            Ok(n) => {
                                if sock_w.write_all(&buf[..n]).is_err() {
                                    return;
                                }
                            }
                            Err(_) => {
                                ack = false;
                                break;
                            }
                        }
                    }
                    if ack {
                        let _ = ctl.write_all(&drain_frame(DRAIN_ACK, generation));
                    }
                }
                Ok(_) | Err(_) => ctl_open = false,
            }
        }
    }
}

#[cfg(test)]
struct TestRendezvous {
    entered: std::sync::mpsc::Sender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
impl TestRendezvous {
    fn new() -> (
        Self,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered, observed) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        (Self { entered, resume }, observed, release)
    }

    fn hold(self) -> bool {
        self.entered.send(()).is_ok() && self.resume.recv().is_ok()
    }
}

#[derive(Default)]
struct DrainControl {
    stream: Option<UnixStream>,
    next_generation: u64,
    #[cfg(test)]
    before_deadline: Option<TestRendezvous>,
    #[cfg(test)]
    next_now: Option<Instant>,
}

/// Ask the forwarder to flush what it read from `pipe-pane`. A matching
/// generation ACK plus an empty `FIONREAD` queue form one snapshot boundary.
fn drain_forwarder(control: &Mutex<DrainControl>) -> bool {
    drain_forwarder_with_io(control, Instant::now, |stream| read_drain_frame(stream))
}

fn drain_forwarder_with_io(
    control: &Mutex<DrainControl>,
    now: impl Fn() -> Instant,
    mut read_frame: impl FnMut(&mut UnixStream) -> std::io::Result<(u8, u64)>,
) -> bool {
    use std::io::Write;

    let Ok(mut control) = control.lock() else {
        return false;
    };
    #[cfg(test)]
    let before_deadline = control.before_deadline.take();
    #[cfg(test)]
    let fixed_now = control.next_now.take();
    #[cfg(test)]
    let now = || fixed_now.unwrap_or_else(&now);
    let generation = control.next_generation;
    control.next_generation = control.next_generation.wrapping_add(1);
    let Some(stream) = control.stream.as_mut() else {
        return false;
    };
    #[cfg(test)]
    if before_deadline.is_some_and(|boundary| !boundary.hold()) {
        return false;
    }
    let deadline = now() + Duration::from_millis(100);
    if stream
        .write_all(&drain_frame(DRAIN_PROBE, generation))
        .is_err()
    {
        return false;
    }
    loop {
        let Some(remaining) = deadline.checked_duration_since(now()) else {
            return false;
        };
        if stream.set_read_timeout(Some(remaining)).is_err() {
            return false;
        }
        match read_frame(&mut *stream) {
            Ok((DRAIN_ACK, ack_generation)) if ack_generation == generation => return true,
            Ok(_) => continue,
            Err(_) => return false,
        }
    }
}

/// Live channels by session name, held weakly.
static REGISTRY: LazyLock<Mutex<HashMap<String, Weak<VtChannel>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Per-session arm locks: a second `pipe-pane` replaces the first, and the
/// losing channel's `Drop` would then disable the survivor's pipe. Separate
/// from `REGISTRY`'s lock, which keystrokes take. Pruned when idle.
static ARM_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `pipe-pane` allows one command per pane, so OSC 52 observers are shared too.
static OSC52_REGISTRY: LazyLock<Mutex<HashMap<String, Weak<Osc52Channel>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OSC52_ARM_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SOCK_COUNTER: AtomicU64 = AtomicU64::new(0);
static PIPE_OWNER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Base for chunk-arrival millis, read by the capture worker's debounce.
static CHUNK_CLOCK: LazyLock<Instant> = LazyLock::new(Instant::now);

fn chunk_now_ms() -> u64 {
    CHUNK_CLOCK.elapsed().as_millis() as u64
}

/// Lease identity for one armed pipe generation; a dead channel and its
/// replacement can share a session name.
fn new_pipe_owner_id() -> String {
    format!(
        "pipe-{}-{}",
        std::process::id(),
        PIPE_OWNER_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A pair an in-process poller parks on, poked after every grid change and on
/// death.
pub(crate) type ChangeWakeup = Arc<(Mutex<u64>, Condvar)>;

/// Notify under the pair's mutex so the wake cannot fall between a parker's
/// lock and wait.
fn notify_change_wakeup(slot: &Mutex<Option<ChangeWakeup>>) {
    let pair = match slot.lock() {
        Ok(guard) => guard.clone(),
        Err(_) => None,
    };
    if let Some(pair) = pair {
        if let Ok(mut generation) = pair.0.lock() {
            *generation = generation.wrapping_add(1);
            pair.1.notify_one();
        }
    }
}

/// Grid scrollback and seed history; tmux's default `history-limit`.
pub(crate) const SCROLLBACK_LINES: usize = 2000;

fn lookup(session: &str) -> Option<Arc<VtChannel>> {
    REGISTRY
        .lock()
        .unwrap()
        .get(session)
        .and_then(Weak::upgrade)
}

fn lookup_osc52(session: &str) -> Option<Arc<Osc52Channel>> {
    OSC52_REGISTRY
        .lock()
        .unwrap()
        .get(session)
        .and_then(Weak::upgrade)
}

/// DECCKM as the live grid last saw it, whether or not input rides the socket.
pub(crate) fn cursor_mode(session: &str) -> Option<bool> {
    lookup(session)
        .filter(|c| c.is_alive())
        .map(|c| c.app_cursor.load(Ordering::Relaxed))
}

/// DECCKM of a live input-capable channel. `Some` means all pane input must go
/// through [`try_send_input`], never `send-keys`; `None` falls back.
pub(crate) fn input_mode(session: &str) -> Option<bool> {
    lookup(session)
        .filter(|c| c.input && c.is_alive())
        .map(|c| c.app_cursor.load(Ordering::Relaxed))
}

/// Deliver raw `bytes` to `session`'s pane via its channel. Returns `true` if
/// written, `false` if no channel is armed or the forwarder hasn't connected.
pub(crate) fn try_send_input(session: &str, bytes: &[u8]) -> bool {
    lookup(session)
        .map(|c| c.write_input(bytes))
        .unwrap_or(false)
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `(pane_width, pane_height, cursor_x, cursor_y)` in one fork, for
/// [`VtChannel::reconcile_grid`]'s resize trigger and drift detector.
fn pane_size_cursor(
    target: &str,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> Option<(u16, u16, u16, u16)> {
    let mut command = crate::tmux::tmux_command();
    command.args([
        "display-message",
        "-p",
        "-t",
        target,
        "-F",
        "#{pane_width} #{pane_height} #{cursor_x} #{cursor_y}",
    ]);
    let out = deadline.run(&mut command).ok()?;
    if !out.status.success() {
        return None;
    }
    parse_size_cursor(&String::from_utf8_lossy(&out.stdout))
}

/// `None` for a short or non-numeric line: a half-read cursor would look like
/// drift and reseed every second.
fn parse_size_cursor(raw: &str) -> Option<(u16, u16, u16, u16)> {
    let mut it = raw.split_whitespace();
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    let cx = it.next()?.parse().ok()?;
    let cy = it.next()?.parse().ok()?;
    Some((w, h, cx, cy))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridReconcile {
    InSync,
    Resize,
    /// Cursor disagrees for the first time; remember the generation.
    ArmDrift,
    /// Still disagrees a pass later with no output in between: reseed.
    Reseed,
}

/// Geometry wins. Otherwise the cursor is the resync for a missed or doubled
/// byte, confirmed across two passes at an unchanged `grid_gen` so a probe
/// racing output (or a busy agent) never reseeds.
fn reconcile_step(
    tmux: (u16, u16, u16, u16),
    grid: (u16, u16, u16, u16),
    pending: Option<u64>,
    grid_gen: u64,
) -> GridReconcile {
    let (tw, th, tcx, tcy) = tmux;
    let (gw, gh, gcx, gcy) = grid;
    if (tw, th) != (gw, gh) {
        return GridReconcile::Resize;
    }
    // Compare the last column as one bucket: a pending wrap reports
    // `cursor_x == pane_width`, but a seeded CUP clamps to `cols - 1`.
    let last_col = tw.saturating_sub(1);
    if (tcx.min(last_col), tcy) == (gcx.min(last_col), gcy) {
        return GridReconcile::InSync;
    }
    match pending {
        Some(gen) if gen == grid_gen => GridReconcile::Reseed,
        _ => GridReconcile::ArmDrift,
    }
}

/// Pane state `capture-pane -e` cannot carry: modes, cursor and DECTCEM.
/// `PartialEq` guards the seed: the probes before and after the capture must
/// agree (`history_size` and geometry detect scroll and resize).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct PaneSeedState {
    alt: bool,
    mouse: bool,
    mouse_sgr: bool,
    /// `#{mouse_all_flag}`: any-event tracking (DEC 1003), which the hover
    /// forwarding keys off (#2904).
    mouse_all: bool,
    /// Visible-screen cursor, 0-based.
    cursor_x: u16,
    cursor_y: u16,
    cursor_visible: bool,
    /// DECCKM, so arrows encode correctly before the app re-emits the mode.
    app_cursor: bool,
    history_size: u32,
    pane_height: u16,
    pane_width: u16,
}

/// Shared by both seed probes; field order matches [`parse_seed_state`].
const SEED_STATE_FMT: &str = "#{alternate_on} #{mouse_any_flag} #{mouse_sgr_flag} #{mouse_all_flag} #{cursor_x} #{cursor_y} #{cursor_flag} #{keypad_cursor_flag} #{history_size} #{pane_height} #{pane_width}";

/// Missing or malformed fields fall back to defaults.
fn parse_seed_state(line: &str) -> PaneSeedState {
    let mut it = line.split_whitespace();
    let alt = it.next().map(|f| f != "0").unwrap_or(false);
    let mouse = it.next().map(|f| f != "0").unwrap_or(false);
    let mouse_sgr = it.next().map(|f| f != "0").unwrap_or(false);
    let mouse_all = it.next().map(|f| f != "0").unwrap_or(false);
    let cursor_x = it.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let cursor_y = it.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let cursor_visible = it.next().map(|f| f != "0").unwrap_or(true);
    let app_cursor = it.next().map(|f| f != "0").unwrap_or(false);
    let history_size = it.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let pane_height = it.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    let pane_width = it.next().and_then(|f| f.parse().ok()).unwrap_or(0);
    PaneSeedState {
        alt,
        mouse,
        mouse_sgr,
        mouse_all,
        cursor_x,
        cursor_y,
        cursor_visible,
        app_cursor,
        history_size,
        pane_height,
        pane_width,
    }
}

/// Modes and cursor in one `display-message` round-trip.
fn pane_seed_state(
    target: &str,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> Option<PaneSeedState> {
    let mut command = crate::tmux::tmux_command();
    command.args(["display-message", "-p", "-t", target, "-F", SEED_STATE_FMT]);
    let out = deadline.run(&mut command).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_seed_state(&String::from_utf8_lossy(&out.stdout)))
}

/// Bare LF to CRLF so seeded rows start at column 0.
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

/// Channel lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VtRefreshResult {
    Refreshed,
    Busy,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum VtLifecycle {
    /// The pipe was armed but its reader has not connected yet.
    Starting,
    /// The reader is connected and the grid may be sampled.
    Live,
    /// The forwarder disconnected and callers may try to recover.
    Failed,
}

impl VtLifecycle {
    fn load(state: &AtomicU8) -> Self {
        match state.load(Ordering::Acquire) {
            x if x == Self::Live as u8 => Self::Live,
            x if x == Self::Failed as u8 => Self::Failed,
            _ => Self::Starting,
        }
    }

    fn store(self, state: &AtomicU8) {
        state.store(self as u8, Ordering::Release);
    }

    fn fail(state: &AtomicU8) {
        let mut current = state.load(Ordering::Acquire);
        loop {
            if current == Self::Failed as u8 {
                return;
            }
            match state.compare_exchange_weak(
                current,
                Self::Failed as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(next) => current = next,
            }
        }
    }
}
#[derive(Clone, Copy)]
struct SeedGuard<'a> {
    chunk: Option<(&'a AtomicU64, &'a AtomicU64, u64)>,
    pipe: Option<&'a UnixStream>,
}

/// Grid generation and received chunk sequence, sampled for one capture.
type SeedFenceSample = (Option<u64>, Option<u64>);

struct SeedInstallFence<'a> {
    snapshot: Option<&'a Mutex<()>>,
    socket: Option<&'a Mutex<Option<UnixStream>>>,
    control: Option<&'a Mutex<DrainControl>>,
}

struct DrainedSeedGuard<'a> {
    guard: SeedGuard<'a>,
    control: &'a Mutex<DrainControl>,
}

/// The four channel handles one seed writes into.
struct SeedSink<'a> {
    parser: &'a Mutex<vt100::Parser>,
    app_cursor: &'a AtomicBool,
    grid_gen: &'a AtomicU64,
    links: &'a LinkTable,
}

/// Only a landed swap moves the recorded geometry.
fn refresh_commits_geometry(result: VtRefreshResult) -> bool {
    result == VtRefreshResult::Refreshed
}

fn seed_parser(
    target: &str,
    sink: SeedSink<'_>,
    guarded: bool,
    size: (u16, u16),
    deadline: &crate::tmux::TmuxCommandDeadline,
    chunk: Option<(&AtomicU64, &AtomicU64)>,
    fence: SeedInstallFence<'_>,
) -> VtRefreshResult {
    seed_parser_with(sink, guarded, size, chunk, fence, |sample| {
        capture_seed_stream(target, size, deadline, sample)
    })
}

/// Capture and install behind the fence; `guarded` enables the generation guard.
fn seed_parser_with(
    sink: SeedSink<'_>,
    guarded: bool,
    size: (u16, u16),
    chunk: Option<(&AtomicU64, &AtomicU64)>,
    fence: SeedInstallFence<'_>,
    capture: impl FnOnce(&mut dyn FnMut() -> SeedFenceSample) -> Option<(Vec<u8>, SeedFenceSample)>,
) -> VtRefreshResult {
    let grid_gen = sink.grid_gen;
    let mut sample = || -> SeedFenceSample {
        (
            guarded.then(|| grid_gen.load(Ordering::Relaxed)),
            chunk.map(|(received, _)| received.load(Ordering::Acquire)),
        )
    };
    let Some((stream, (since, expected))) = capture(&mut sample) else {
        return VtRefreshResult::Failed;
    };
    let guard = SeedGuard {
        chunk: chunk
            .zip(expected)
            .map(|((received, settled), expected)| (received, settled, expected)),
        pipe: None,
    };
    install_seeded_parser(sink, since, &stream, size, guard, fence)
}

/// Install a captured snapshot behind the fence.
fn install_seeded_parser(
    sink: SeedSink<'_>,
    since: Option<u64>,
    stream: &[u8],
    size: (u16, u16),
    guard: SeedGuard<'_>,
    fence: SeedInstallFence<'_>,
) -> VtRefreshResult {
    let (Some(snapshot), Some(socket), Some(control)) =
        (fence.snapshot, fence.socket, fence.control)
    else {
        return swap_seeded_parser(sink, since, stream, size, guard);
    };
    let Ok(_snapshot) = snapshot.lock() else {
        return VtRefreshResult::Failed;
    };
    // Clone the socket and release the mutex before draining: the drain waits up
    // to 100 ms and `write_input` needs the same mutex.
    let pipe = match socket.lock() {
        Ok(guard) => match guard.as_ref() {
            Some(stream) => match stream.try_clone() {
                Ok(clone) => Some(clone),
                Err(_) => return VtRefreshResult::Failed,
            },
            None => None,
        },
        Err(_) => return VtRefreshResult::Failed,
    };
    let guard = SeedGuard {
        pipe: pipe.as_ref(),
        ..guard
    };
    swap_drained_seeded_parser(
        sink,
        since,
        stream,
        size,
        DrainedSeedGuard { guard, control },
    )
}
/// Capture the pane and weave its modes and cursor into one replayable stream.
/// `sample` runs just before the capture fork.
fn capture_seed_stream<S>(
    target: &str,
    size: (u16, u16),
    deadline: &crate::tmux::TmuxCommandDeadline,
    sample: impl FnMut() -> S,
) -> Option<(Vec<u8>, S)> {
    let (cols, rows) = size;
    let (body, state, sampled) = capture_seed_snapshot(target, (cols, rows), deadline, sample)?;
    Some((assemble_seed_stream(&body, &state, rows), sampled))
}

fn pipe_has_unread_bytes(pipe: &UnixStream) -> bool {
    let mut unread: libc::c_int = 0;
    // FIONREAD writes one c_int through this valid pointer without consuming
    // the socket's receive queue.
    unsafe { libc::ioctl(pipe.as_raw_fd(), libc::FIONREAD, &mut unread) != 0 || unread > 0 }
}

/// Replace `parser` with a grid built from `stream`, unless the reader applied
/// a chunk since `since`, has an unsettled chunk, or unread bytes remain. A raced
/// swap is abandoned: the old parser holds the newer output. `since = None`
/// disables only the generation guard.
fn swap_seeded_parser(
    sink: SeedSink<'_>,
    since: Option<u64>,
    stream: &[u8],
    size: (u16, u16),
    guard: SeedGuard<'_>,
) -> VtRefreshResult {
    let SeedSink {
        parser,
        app_cursor,
        grid_gen,
        links,
    } = sink;
    let Ok(mut p) = parser.lock() else {
        return VtRefreshResult::Failed;
    };
    if since.is_some_and(|generation| generation != grid_gen.load(Ordering::Relaxed))
        || guard.chunk.is_some_and(|(received, settled, expected)| {
            received.load(Ordering::Acquire) != expected
                || settled.load(Ordering::Acquire) != expected
        })
        || guard.pipe.is_some_and(pipe_has_unread_bytes)
    {
        return VtRefreshResult::Busy;
    }
    let (cols, rows) = size;
    *p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
    p.process(stream);
    app_cursor.store(p.screen().application_cursor(), Ordering::Relaxed);
    // Under the parser lock so the grid and its links are installed together.
    reconcile_links(links, crate::tmux::osc8::extract_links(stream));
    grid_gen.fetch_add(1, Ordering::Relaxed);
    VtRefreshResult::Refreshed
}

/// Install only after the forwarder acknowledged its pre-capture input.
fn swap_drained_seeded_parser(
    sink: SeedSink<'_>,
    since: Option<u64>,
    stream: &[u8],
    size: (u16, u16),
    drained_guard: DrainedSeedGuard<'_>,
) -> VtRefreshResult {
    if !drain_forwarder(drained_guard.control) {
        return VtRefreshResult::Busy;
    }
    swap_seeded_parser(sink, since, stream, size, drained_guard.guard)
}
/// Probe/capture/probe rounds before settling for the last snapshot.
const SEED_PROBE_ATTEMPTS: usize = 3;

const SEED_RETRY_SETTLE: Duration = Duration::from_millis(5);

/// Snapshot-and-install cycles when a chunk races the capture.
const SEED_INSTALL_ATTEMPTS: usize = 8;
const SEED_INSTALL_RETRY: Duration = Duration::from_millis(20);

/// A capture body plus a [`PaneSeedState`] known to describe the same instant:
/// probe, then capture and re-probe in one tmux invocation, retrying while the
/// probes disagree. The last attempt's pairing is used if it never settles.
fn capture_seed_snapshot<S>(
    target: &str,
    want: (u16, u16),
    deadline: &crate::tmux::TmuxCommandDeadline,
    mut sample: impl FnMut() -> S,
) -> Option<(Vec<u8>, PaneSeedState, S)> {
    let seed_start = format!("-{SCROLLBACK_LINES}");
    let mut last: Option<(Vec<u8>, PaneSeedState, S)> = None;
    for attempt in 0..SEED_PROBE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(SEED_RETRY_SETTLE);
        }
        // A failure mid-retry falls back to the last self-consistent snapshot.
        let Some(pre) = pane_seed_state(target, deadline) else {
            break;
        };
        // The alternate screen has no scrollback; `-N` keeps styled trailing fills.
        let mut args = vec!["capture-pane", "-t", target, "-p", "-e", "-N"];
        if !pre.alt {
            args.extend_from_slice(&["-S", &seed_start]);
        }
        args.extend_from_slice(&[
            ";",
            "display-message",
            "-p",
            "-t",
            target,
            "-F",
            SEED_STATE_FMT,
        ]);
        let mut command = crate::tmux::tmux_command();
        command.args(&args);
        // tmux parses pane output before running a later command, so every chunk the
        // reader already holds is in this capture.
        let sampled = sample();
        let Ok(out) = deadline.run(&mut command) else {
            break;
        };
        if !out.status.success() {
            break;
        }
        let (body, probe_line) = split_seed_capture(&out.stdout);
        // A chained invocation can exit 0 with the probe dropped.
        if !is_probe_line(probe_line) {
            break;
        }
        let post = parse_seed_state(probe_line);
        let agreed = pre == post;
        // A capture at the wanted geometry is worth one more probe.
        let at_want = (post.pane_width, post.pane_height) == want;
        last = Some((body.to_vec(), post, sampled));
        if agreed && at_want {
            return last;
        }
    }
    if let Some((_, state, _)) = last.as_ref() {
        tracing::debug!(
            %target,
            attempts = SEED_PROBE_ATTEMPTS,
            probe = ?(state.pane_width, state.pane_height),
            want = ?want,
            "vt seed: no settled snapshot at the target geometry; seeding from last"
        );
    }
    last
}

/// Whether a line is the [`SEED_STATE_FMT`] probe: exact field count, all
/// numeric.
fn is_probe_line(line: &str) -> bool {
    let expected = SEED_STATE_FMT.split_whitespace().count();
    let mut tokens = 0usize;
    for tok in line.split_whitespace() {
        if tok.bytes().any(|b| !b.is_ascii_digit()) {
            return false;
        }
        tokens += 1;
    }
    tokens == expected
}

/// Split chained output into the capture body and the trailing probe line.
fn split_seed_capture(raw: &[u8]) -> (&[u8], &str) {
    let trimmed = raw.strip_suffix(b"\n").unwrap_or(raw);
    match trimmed.iter().rposition(|&b| b == b'\n') {
        Some(idx) => (
            &trimmed[..=idx],
            std::str::from_utf8(&trimmed[idx + 1..]).unwrap_or(""),
        ),
        None => (b"", std::str::from_utf8(trimmed).unwrap_or("")),
    }
}

/// The seed stream: DEC mode SETs, the CRLF body (minus its final terminator,
/// which would scroll the screen), then an absolute CUP and DECTCEM.
fn assemble_seed_stream(body: &[u8], state: &PaneSeedState, rows: u16) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(body.len() + 32);
    if state.alt {
        out.extend_from_slice(b"\x1b[?1049h");
    }
    // 1003 subsumes 1000; replay whichever the app asked for.
    if state.mouse_all {
        out.extend_from_slice(b"\x1b[?1003h");
    } else if state.mouse {
        out.extend_from_slice(b"\x1b[?1000h");
    }
    if state.mouse_sgr {
        out.extend_from_slice(b"\x1b[?1006h");
    }
    if state.app_cursor {
        out.extend_from_slice(b"\x1b[?1h");
    }
    out.extend_from_slice(&lf_to_crlf(strip_trailing_row_terminator(body)));
    // 1-based CUP, clamped to the grid.
    let cy = seeded_cursor_row(body, state, rows).min(rows.saturating_sub(1)) + 1;
    let cx = state.cursor_x + 1;
    out.extend_from_slice(format!("\x1b[{cy};{cx}H").as_bytes());
    out.extend_from_slice(if state.cursor_visible {
        b"\x1b[?25h"
    } else {
        b"\x1b[?25l"
    });
    out
}

/// The grid row for tmux's `cursor_y`, bottom-anchored so a body captured at a
/// taller pane height (a reseed racing a resize) keeps the cursor with its
/// content. Keep in step with `tui::home::render::map_live_preview_cursor`.
fn seeded_cursor_row(body: &[u8], state: &PaneSeedState, rows: u16) -> u16 {
    if state.pane_height == 0 {
        return state.cursor_y;
    }
    let fed = strip_trailing_row_terminator(body);
    let body_rows = if fed.is_empty() {
        0
    } else {
        u16::try_from(fed.iter().filter(|&&b| b == b'\n').count() + 1).unwrap_or(u16::MAX)
    };
    // Rows of the body the grid still shows; the rest scrolled into history.
    let visible = body_rows.min(rows);
    visible.saturating_sub(state.pane_height.saturating_sub(state.cursor_y))
}

/// Drop the single trailing line terminator; padded blank rows stay.
fn strip_trailing_row_terminator(raw: &[u8]) -> &[u8] {
    match raw.split_last() {
        Some((b'\n', rest)) => match rest.split_last() {
            Some((b'\r', rest2)) => rest2,
            _ => rest,
        },
        _ => raw,
    }
}

/// tmux >= 3.4 is required to arm a channel at all. Cached.
fn tmux_supports_pipe_pane_io(deadline: &crate::tmux::TmuxCommandDeadline) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cached_tmux_support(&SUPPORTED, || {
        parse_tmux_pipe_support(&tmux_version(deadline)?)
    })
}

/// Input on the pipe's `-I` side needs a tmux newer than 3.7a: earlier, a write
/// to the pipe of a pane dead under `remain-on-exit` crashes the tmux server.
fn tmux_supports_pipe_pane_input(deadline: &crate::tmux::TmuxCommandDeadline) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cached_tmux_support(&SUPPORTED, || {
        parse_tmux_pipe_input_support(&tmux_version(deadline)?)
    })
}

fn tmux_version(deadline: &crate::tmux::TmuxCommandDeadline) -> Option<String> {
    let mut command = crate::tmux::tmux_command();
    command.arg("-V");
    let out = deadline.run(&mut command).ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn cached_tmux_support(
    cache: &std::sync::OnceLock<bool>,
    probe: impl FnOnce() -> Option<bool>,
) -> bool {
    if let Some(supported) = cache.get() {
        return *supported;
    }
    let Some(supported) = probe() else {
        return false;
    };
    let _ = cache.set(supported);
    supported
}

fn parse_tmux_pipe_support(version: &str) -> Option<bool> {
    parse_tmux_version(version).map(|v| v >= (3, 4))
}

fn parse_tmux_pipe_input_support(version: &str) -> Option<bool> {
    parse_tmux_version(version).map(|v| v >= (3, 8))
}

fn parse_tmux_version(version: &str) -> Option<(u32, u32)> {
    let digits: String = version
        .trim()
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor))
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
        position_reliable: true,
        composite_pane0: None,
    }
}

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

/// Styled blanks (a background fill) are visible content.
fn cell_has_style(cell: &vt100::Cell) -> bool {
    cell.bold()
        || cell.dim()
        || cell.italic()
        || cell.underline()
        || cell.inverse()
        || !matches!(cell.fgcolor(), vt100::Color::Default)
        || !matches!(cell.bgcolor(), vt100::Color::Default)
}

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

/// One grid row as SGR plus literal characters. vt100's `rows_formatted` uses
/// cursor movement for blank runs, which `ansi_to_tui` ignores.
fn row_to_ansi(screen: &vt100::Screen, row: u16, cols: u16) -> String {
    let last = row_last_col(screen, row, cols);
    row_to_ansi_upto(screen, row, last)
}

/// Display columns up to the last content cell (styled blanks count), so a wide
/// trailing glyph counts both of its columns.
fn row_last_col(screen: &vt100::Screen, row: u16, cols: u16) -> u16 {
    let mut last = 0u16;
    for col in 0..cols {
        if let Some(cell) = screen.cell(row, col) {
            if cell.has_contents() || cell_has_style(cell) {
                let width = if cell.is_wide() { 2 } else { 1 };
                last = col.saturating_add(width).min(cols);
            }
        }
    }
    last
}

fn row_to_ansi_upto(screen: &vt100::Screen, row: u16, last: u16) -> String {
    let mut out = String::new();
    let mut cur_sgr: Option<String> = None;
    let mut col = 0u16;
    while col < last {
        let Some(cell) = screen.cell(row, col) else {
            out.push(' ');
            col += 1;
            continue;
        };
        if cell.is_wide_continuation() {
            col += 1;
            continue;
        }
        let sgr = cell_sgr(cell);
        if cur_sgr.as_deref() != Some(sgr.as_str()) {
            // Reset first so the previous cell's attributes never bleed.
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

/// Render one pane's capture as exactly `rows` rows padded to `cols` columns,
/// via a parser so no SGR state leaks across a pane boundary when rows are
/// concatenated.
pub(crate) fn capture_rows_padded(raw: &[u8], cols: u16, rows: u16) -> Vec<String> {
    let cols = cols.max(1);
    let rows = rows.max(1);
    // At least two rows: vt100 0.16 panics when content wraps on a one-row grid.
    let mut parser = vt100::Parser::new(rows.max(2), cols, 0);
    parser.process(&lf_to_crlf(strip_trailing_row_terminator(raw)));

    let screen = parser.screen();
    (0..rows)
        .map(|row| {
            let last = row_last_col(screen, row, cols);
            let mut out = row_to_ansi_upto(screen, row, last);
            if last < cols {
                out.push_str("\x1b[0m");
                out.extend(std::iter::repeat_n(' ', (cols - last) as usize));
            }
            out
        })
        .collect()
}

/// The last `max_lines` rows of scrollback plus screen as ANSI, and the
/// scrollback depth, read at successive scrollback offsets like
/// `capture-pane -S -<lines>`.
fn grid_content(
    parser: &mut vt100::Parser,
    max_lines: usize,
    cols: u16,
    rows: u16,
) -> (String, usize) {
    let h = (rows as usize).max(1);
    let saved = parser.screen().scrollback();
    parser.screen_mut().set_scrollback(usize::MAX >> 4);
    let total_sb = parser.screen().scrollback();
    let total = total_sb + h;
    let want = max_lines.clamp(h.min(total), total);
    let target_low = total - want;

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
        content.push_str("\x1b[0m\n");
    }
    (content, total_sb)
}

/// State the reader thread owns, as a struct so tests can drive the loop.
struct ReaderCtx {
    #[cfg(test)]
    snapshot_contended: Option<std::sync::mpsc::Sender<()>>,
    parser: Arc<Mutex<vt100::Parser>>,
    stop: Arc<AtomicBool>,
    seeded: Arc<AtomicBool>,
    snapshot: Arc<Mutex<()>>,
    stream: Arc<Mutex<Option<UnixStream>>>,
    app_cursor: Arc<AtomicBool>,
    lifecycle: Arc<AtomicU8>,
    wakeup: Arc<Mutex<Option<ChangeWakeup>>>,
    clipboard: Arc<Mutex<Option<String>>>,
    chunk_seq: Arc<AtomicU64>,
    /// Read sequences no longer waiting on the parser; a seed commits only when
    /// this equals its baseline.
    settled_chunk_seq: Arc<AtomicU64>,
    last_chunk_ms: Arc<AtomicU64>,
    prev_gap_ms: Arc<AtomicU64>,
    /// Bumped per parsed chunk (and per seed) to key the sample cache.
    grid_gen: Arc<AtomicU64>,
    links: Arc<LinkTable>,
    signals: Arc<ViewerSignals>,
}

fn run_drain_listener(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<DrainControl>>,
) {
    let Ok((conn, _)) = listener.accept() else {
        return;
    };
    if !stop.load(Ordering::Relaxed) {
        control.lock().unwrap().stream = Some(conn);
    }
}

/// Fold new links in, newest last; a repeated target moves to the end.
fn record_links(slot: &LinkTable, found: Vec<PaneLink>) {
    if found.is_empty() {
        return;
    }
    let Ok(mut table) = slot.table.lock() else {
        return;
    };
    let before: Vec<PaneLink> = table.iter().cloned().collect();
    for link in found {
        table.retain(|held| *held != link);
        table.push_back(link);
        while table.len() > crate::tmux::osc8::MAX_PANE_LINKS {
            table.pop_front();
        }
    }
    // Reordering counts as a change: the newest entry wins overlaps.
    if before.iter().ne(table.iter()) {
        slot.generation.fetch_add(1, Ordering::Release);
    }
}

/// Replace the table with an accepted snapshot's links, which are the complete
/// current set. Called under the parser lock and snapshot fence, so the table
/// and its frame install together.
fn reconcile_links(slot: &LinkTable, found: Vec<PaneLink>) {
    let Ok(mut table) = slot.table.lock() else {
        return;
    };
    let mut next: VecDeque<PaneLink> = VecDeque::new();
    for link in found {
        if !next.contains(&link) {
            next.push_back(link);
        }
    }
    while next.len() > crate::tmux::osc8::MAX_PANE_LINKS {
        next.pop_front();
    }
    if table.iter().ne(next.iter()) {
        *table = next;
        slot.generation.fetch_add(1, Ordering::Release);
    }
}

/// A link table plus a generation, since the grid can be byte-identical across
/// a target change.
#[derive(Debug, Default)]
pub(crate) struct LinkTable {
    table: Mutex<VecDeque<PaneLink>>,
    generation: AtomicU64,
}

/// OSC 8 links of a live channel, oldest first. A dead channel answers nothing,
/// so its frozen table cannot keep stale labels clickable.
pub(crate) fn pane_links(session: &str) -> Vec<PaneLink> {
    lookup(session)
        .filter(|c| c.lifecycle() == VtLifecycle::Live)
        .and_then(|c| {
            c.links
                .table
                .lock()
                .ok()
                .map(|t| t.iter().cloned().collect())
        })
        .unwrap_or_default()
}

/// Link table change count, gated like [`pane_links`].
pub(crate) fn pane_links_generation(session: &str) -> u64 {
    lookup(session)
        .filter(|c| c.lifecycle() == VtLifecycle::Live)
        .map_or(0, |c| c.links.generation.load(Ordering::Acquire))
}

impl ReaderCtx {
    fn lock_snapshot(&self) -> std::sync::LockResult<std::sync::MutexGuard<'_, ()>> {
        #[cfg(test)]
        if let Some(contended) = &self.snapshot_contended {
            return crate::session::test_support::lock_reporting_contention(&self.snapshot, || {
                let _ = contended.send(());
            });
        }
        self.snapshot.lock()
    }

    fn notify_viewers(&self) {
        notify_change_wakeup(&self.wakeup);
        self.signals.bump_changed();
    }
}

fn stop_and_wake_reader(stop: &AtomicBool, sock_path: &std::path::Path) {
    stop.store(true, Ordering::Relaxed);
    let _ = UnixStream::connect(sock_path);
}

/// Reader loop: accept the forwarder, publish the writable half, then pump pane
/// output into the grid and wake viewers. Exits on EOF, error or `stop`.
fn run_reader(listener: UnixListener, ctx: ReaderCtx, clock: impl Fn() -> u64) {
    run_reader_with_wait(listener, ctx, clock, |fd| unsafe { libc::poll(fd, 1, 200) });
}

fn run_reader_with_wait(
    listener: UnixListener,
    ctx: ReaderCtx,
    clock: impl Fn() -> u64,
    mut wait: impl FnMut(&mut libc::pollfd) -> i32,
) {
    let Ok((conn, _)) = listener.accept() else {
        VtLifecycle::fail(&ctx.lifecycle);
        return;
    };
    if let Ok(w) = conn.try_clone() {
        *ctx.stream.lock().unwrap() = Some(w);
    }
    // Now the live single-writer; `acquire` waits for this.
    VtLifecycle::Live.store(&ctx.lifecycle);
    let mut buf = [0u8; 8192];
    let mut osc52 = Osc52Scanner::new();
    let mut osc8 = Osc8Scanner::new();
    let mut sync = SyncOutputScanner::new();
    let mut sync_events: Vec<bool> = Vec::new();
    while !ctx.stop.load(Ordering::Relaxed) {
        let mut fd = libc::pollfd {
            fd: conn.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = wait(&mut fd);
        if ready == -1 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if ready == 0 {
            continue;
        }
        if fd.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
            continue;
        }
        // The snapshot fence: seeds hold it from drain through parser replacement.
        let Ok(_snapshot) = ctx.lock_snapshot() else {
            break;
        };
        let received = unsafe {
            libc::recv(
                conn.as_raw_fd(),
                buf.as_mut_ptr().cast(),
                buf.len(),
                libc::MSG_DONTWAIT,
            )
        };
        match received {
            0 => break,
            n if n > 0 => {
                let n = n as usize;
                // Track 2026 brackets before anything can publish this chunk.
                sync_events.clear();
                sync.feed(&buf[..n], &mut sync_events);
                let sync_plan = SyncHoldPlan::from_events(&sync_events);
                sync_plan.begin(&ctx.signals, &clock);
                // vt100 drops OSC 52 and live-send has no attached client, so this tap is the
                // only route to the host clipboard; it runs even on pre-seed chunks.
                let copied = osc52.feed(&buf[..n]);
                if let Some(text) = copied.as_ref() {
                    if let Ok(mut guard) = ctx.clipboard.lock() {
                        *guard = Some(text.clone());
                    }
                    ctx.signals.publish_clipboard(text);
                }
                // Claim the read before waiting on the parser so a seed that captured it
                // returns Busy instead of applying it twice.
                let seq = ctx.chunk_seq.fetch_add(1, Ordering::AcqRel);
                // Pre-seed chunks are already in the later snapshot.
                if !ctx.seeded.load(Ordering::Acquire) {
                    // Release a closing bracket here, or the discarded repaint's timestamp leaks.
                    sync_plan.end(&ctx.signals);
                    ctx.settled_chunk_seq.store(seq + 1, Ordering::Release);
                    if copied.is_some() {
                        ctx.notify_viewers();
                    }
                    continue;
                }
                // After the seed gate and inside the fence, so links match accepted bytes.
                record_links(&ctx.links, osc8.feed(&buf[..n]));
                if let Ok(mut p) = ctx.parser.lock() {
                    p.process(&buf[..n]);
                    ctx.app_cursor
                        .store(p.screen().application_cursor(), Ordering::Relaxed);
                    // Bump under the parser lock so guarded swaps see it.
                    ctx.grid_gen.fetch_add(1, Ordering::Relaxed);
                    let now = clock();
                    let prev = ctx.last_chunk_ms.swap(now, Ordering::Relaxed);
                    ctx.prev_gap_ms.store(
                        if seq == 0 {
                            u64::MAX
                        } else {
                            now.saturating_sub(prev)
                        },
                        Ordering::Relaxed,
                    );
                    sync_plan.end(&ctx.signals);
                    ctx.settled_chunk_seq.store(seq + 1, Ordering::Release);
                    // Mid-bracket frames are half drawn; viewers wake on close.
                    if sync_plan.close || !ctx.signals.hold_active_at(clock()) {
                        ctx.notify_viewers();
                    }
                }
            }
            _ => match std::io::Error::last_os_error().kind() {
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => {}
                _ => break,
            },
        }
    }
    // No longer the live single-writer: input and capture fall back.
    VtLifecycle::fail(&ctx.lifecycle);
    ctx.signals.end_hold();
    ctx.notify_viewers();
}

/// One shared pane channel: a vt100 grid fed by `pipe-pane`, plus the socket's
/// writable half for keystrokes.
pub(crate) struct VtChannel {
    name: String,
    /// Armed `-IO` (keystrokes ride the socket) rather than `-O` only.
    input: bool,
    owner_id: String,
    target: String,
    parser: Arc<Mutex<vt100::Parser>>,
    stream: Arc<Mutex<Option<UnixStream>>>,
    app_cursor: Arc<AtomicBool>,
    /// `acquire` only hands out a `Live` channel.
    lifecycle: Arc<AtomicU8>,
    /// One in-process poller wakeup; last registration wins.
    wakeup: Arc<Mutex<Option<ChangeWakeup>>>,
    clipboard: Arc<Mutex<Option<String>>>,
    links: Arc<LinkTable>,
    chunk_seq: Arc<AtomicU64>,
    /// Chunks the reader finished applying, plus the fence for snapshots.
    settled_chunk_seq: Arc<AtomicU64>,
    snapshot: Arc<Mutex<()>>,
    drain: Arc<Mutex<DrainControl>>,
    last_chunk_ms: Arc<AtomicU64>,
    /// Gap between the two most recent chunks, telling a lone echo from a stream.
    prev_gap_ms: Arc<AtomicU64>,
    grid_gen: Arc<AtomicU64>,
    /// The last assembled sample, keyed by generation, window and size. One entry,
    /// so viewers at different sizes evict each other.
    sample_cache: Mutex<Option<SampleCache>>,
    signals: Arc<ViewerSignals>,
    /// A fresh seed may have caught a repaint, so viewers hold the opening frame.
    armed_at: Instant,
    /// Owner-only (0700) directory holding `sock_path`; removed on drop.
    sock_dir: PathBuf,
    sock_path: PathBuf,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    cols: AtomicU16,
    rows: AtomicU16,
    last_size_check: Mutex<Instant>,
    /// Generation a cursor drift was first seen at (see [`reconcile_step`]).
    pending_drift: Mutex<Option<u64>>,
    /// Rate-limits the VT-owner heartbeat refresh.
    last_owner_hb: Mutex<Instant>,
    resize: Mutex<ResizeState>,
}

/// What the parser owes the pane after resizes, under one lock so one viewer
/// can never retire another's outstanding work.
#[derive(Default)]
struct ResizeState {
    /// Geometry the parser must be rebuilt at ([`pack_size`]), 0 when current:
    /// `pipe-pane` carries no reflow redraw.
    owed: u64,
    /// Which declaration installed `owed`; never reused.
    token: u64,
    /// Resizes still running (a count, not a parity).
    in_flight: usize,
    /// Bumped by every declaration and finish, so a probe can detect movement.
    epoch: u64,
}

impl ResizeState {
    fn declare(&mut self, geometry: u64) -> u64 {
        self.epoch += 1;
        self.token += 1;
        self.owed = geometry;
        self.token
    }

    fn begin(&mut self, geometry: u64) -> u64 {
        self.in_flight += 1;
        self.declare(geometry)
    }

    /// Close a resize window, withdrawing `withdrawn`'s declaration only if it is
    /// still current and no other resize is running; leaving it up is safe.
    fn finish(&mut self, withdrawn: Option<u64>) {
        if withdrawn == Some(self.token) && self.in_flight == 1 {
            self.owed = 0;
        }
        self.epoch += 1;
        self.in_flight -= 1;
    }

    /// Nothing moved since `probe` and nothing is moving now.
    fn settled_since(&self, probe: ResizeObservation) -> bool {
        self.in_flight == 0 && self.epoch == probe.epoch
    }
}

#[derive(Clone, Copy)]
struct ResizeObservation {
    epoch: u64,
}

fn pack_size(cols: u16, rows: u16) -> u64 {
    ((cols as u64) << 16) | rows as u64
}

struct SampleCache {
    grid_gen: u64,
    max_lines: usize,
    cols: u16,
    rows: u16,
    content: String,
    cursor: PaneCursor,
}

pub(crate) struct VtSample {
    pub(crate) content: String,
    pub(crate) cursor: Option<PaneCursor>,
    /// Serialized from a half-drawn (mid-bracket) grid, decided under the same
    /// lock that assembled `content`.
    pub(crate) incomplete: bool,
}

impl VtSample {
    fn whole(content: String, cursor: Option<PaneCursor>) -> Self {
        Self {
            content,
            cursor,
            incomplete: false,
        }
    }
}

/// A resize in flight; dropping it closes the window.
pub(crate) struct ResizeInFlight<'a> {
    channel: &'a VtChannel,
    token: u64,
    withdrawn: bool,
}

impl ResizeInFlight<'_> {
    /// The resize never ran: withdraw its expectation on drop (see
    /// [`ResizeState::finish`]).
    pub(crate) fn abandon(mut self) {
        self.withdrawn = true;
    }
}

impl Drop for ResizeInFlight<'_> {
    fn drop(&mut self) {
        self.channel
            .resize_state()
            .finish(self.withdrawn.then_some(self.token));
    }
}

pub(crate) struct VtRowsSample {
    pub(crate) rows: Vec<String>,
    pub(crate) cursor: PaneCursor,
    pub(crate) incomplete: bool,
}

impl VtChannel {
    /// The shared channel for `session`, arming one if none is live. `None` when
    /// tmux is too old or any step fails; callers then use capture/send-keys.
    #[cfg(test)]
    pub(crate) fn acquire(session: &str) -> Option<Arc<VtChannel>> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        Self::acquire_with_deadline(session, &deadline)
    }

    pub(crate) fn acquire_with_deadline(
        session: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<Arc<VtChannel>> {
        // A dead entry (the session was recreated) must not be reused.
        if let Some(ch) = lookup(session) {
            if ch.lifecycle() == VtLifecycle::Live {
                return Some(ch);
            }
        }
        // Serialize arming per session so the loser adopts the winner's channel.
        let arm_lock = ARM_LOCKS
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .clone();
        let result = {
            let _armed = arm_lock.lock().unwrap();
            if let Some(ch) = lookup(session) {
                if ch.lifecycle() == VtLifecycle::Live {
                    Some(ch)
                } else {
                    Self::arm_and_register(session, deadline)
                }
            } else {
                // No `?`: a failure must still prune ARM_LOCKS below.
                Self::arm_and_register(session, deadline)
            }
        };
        drop(arm_lock);
        ARM_LOCKS
            .lock()
            .unwrap()
            .retain(|_, l| Arc::strong_count(l) > 1);
        result
    }

    fn arm_and_register(
        session: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<Arc<VtChannel>> {
        Self::arm(session, deadline).map(|channel| {
            let channel = Arc::new(channel);
            REGISTRY
                .lock()
                .unwrap()
                .insert(session.to_string(), Arc::downgrade(&channel));
            channel
        })
    }

    fn arm(name: &str, deadline: &crate::tmux::TmuxCommandDeadline) -> Option<Self> {
        if !tmux_supports_pipe_pane_io(deadline) {
            return None;
        }
        let target = format!("{name}:^.0");
        let (cols, rows, _, _) = pane_size_cursor(&target, deadline)?;
        // `pipe-pane` is exclusive per pane, so defer to another live VT owner.
        let session = crate::tmux::Session::from_name(name);
        let owner = new_pipe_owner_id();
        if !session.claim_vt_owner_with_deadline(
            &owner,
            crate::tmux::session::VT_OWNER_TTL,
            deadline,
        ) {
            tracing::info!(
                %target,
                pid = std::process::id(),
                "vt: pipe owned by another process; using capture fallback"
            );
            return None;
        }
        let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, SCROLLBACK_LINES)));
        let stop = Arc::new(AtomicBool::new(false));
        let seeded = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(()));
        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let drain: Arc<Mutex<DrainControl>> = Arc::new(Mutex::new(DrainControl::default()));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let lifecycle = Arc::new(AtomicU8::new(VtLifecycle::Starting as u8));
        let clipboard: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let links: Arc<LinkTable> = Arc::new(LinkTable::default());
        // Owner-only directory: other users must not reach the pane socket (on BSDs
        // the socket's own mode is ignored).
        let n = SOCK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let sock_dir = std::env::temp_dir().join(format!("aoe-vt-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&sock_dir);
        let setup = || -> Option<(PathBuf, UnixListener, PathBuf, UnixListener)> {
            std::fs::create_dir_all(&sock_dir).ok()?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700)).ok()?;
            }
            let sock_path = sock_dir.join("s.sock");
            let control_path = sock_dir.join("c.sock");
            Some((
                sock_path.clone(),
                UnixListener::bind(sock_path).ok()?,
                control_path.clone(),
                UnixListener::bind(control_path).ok()?,
            ))
        };
        let Some((sock_path, listener, control_path, control_listener)) = setup() else {
            let _ = std::fs::remove_dir_all(&sock_dir);
            session.release_vt_owner_with_deadline(&owner, deadline);
            return None;
        };
        let Some(exe) = std::env::current_exe().ok() else {
            let _ = std::fs::remove_dir_all(&sock_dir);
            session.release_vt_owner_with_deadline(&owner, deadline);
            return None;
        };
        let wakeup: Arc<Mutex<Option<ChangeWakeup>>> = Arc::new(Mutex::new(None));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let last_chunk_ms = Arc::new(AtomicU64::new(0));
        let prev_gap_ms = Arc::new(AtomicU64::new(u64::MAX));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let signals = Arc::new(ViewerSignals::new());
        let reader = {
            let ctx = ReaderCtx {
                #[cfg(test)]
                snapshot_contended: None,
                parser: parser.clone(),
                stop: stop.clone(),
                seeded: seeded.clone(),
                snapshot: snapshot.clone(),
                stream: stream.clone(),
                app_cursor: app_cursor.clone(),
                lifecycle: lifecycle.clone(),
                wakeup: wakeup.clone(),
                clipboard: clipboard.clone(),
                links: links.clone(),
                chunk_seq: chunk_seq.clone(),
                settled_chunk_seq: settled_chunk_seq.clone(),
                last_chunk_ms: last_chunk_ms.clone(),
                prev_gap_ms: prev_gap_ms.clone(),
                grid_gen: grid_gen.clone(),
                signals: signals.clone(),
            };
            std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms))
        };

        let pipe_cmd = format!(
            "{} __vt-pipe {}",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&sock_path.to_string_lossy())
        );
        let input = tmux_supports_pipe_pane_input(deadline);
        let flags = if input { "-IO" } else { "-O" };
        let armed = session.arm_vt_pipe_if_owner_with_deadline(&owner, flags, &pipe_cmd, deadline);
        if !armed {
            tracing::warn!(%target, "vt: tmux pipe-pane failed; falling back to capture");
            stop_and_wake_reader(&stop, &sock_path);
            session.release_vt_pipe_owner_with_deadline(&owner, deadline);
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&sock_dir);
            return None;
        }
        let control_stop = stop.clone();
        let control_drain = drain.clone();
        std::thread::spawn(move || {
            run_drain_listener(control_listener, control_stop, control_drain)
        });

        // Publish only once the forwarder connects, or early keystrokes would be
        // dropped instead of falling back to `send-keys`.
        let connect_deadline = Instant::now() + Duration::from_millis(500);
        while VtLifecycle::load(&lifecycle) != VtLifecycle::Live
            || drain.lock().unwrap().stream.is_none()
        {
            if Instant::now() >= connect_deadline {
                tracing::warn!(%target, "vt: forwarder did not connect; falling back to capture");
                stop_and_wake_reader(&stop, &sock_path);
                let _ = UnixStream::connect(&control_path);
                session.release_vt_pipe_owner_with_deadline(&owner, deadline);
                let _ = reader.join();
                let _ = std::fs::remove_dir_all(&sock_dir);
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        seeded.store(true, Ordering::Release);
        // A busy pane often races the seed (Busy), so retry; Failed is terminal.
        let mut seed_result = VtRefreshResult::Failed;
        for attempt in 0..SEED_INSTALL_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(SEED_INSTALL_RETRY);
            }
            seed_result = seed_parser(
                &target,
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &links,
                },
                false,
                (cols, rows),
                deadline,
                Some((&chunk_seq, &settled_chunk_seq)),
                SeedInstallFence {
                    snapshot: Some(&snapshot),
                    socket: Some(&stream),
                    control: Some(&drain),
                },
            );
            match seed_result {
                VtRefreshResult::Refreshed | VtRefreshResult::Failed => break,
                VtRefreshResult::Busy => {}
            }
        }
        if seed_result != VtRefreshResult::Refreshed {
            tracing::warn!(
                %target,
                result = ?seed_result,
                "vt: initial seed failed; falling back to capture"
            );
            stop_and_wake_reader(&stop, &sock_path);
            let _ = UnixStream::connect(&control_path);
            session.release_vt_pipe_owner_with_deadline(&owner, deadline);
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&sock_dir);
            return None;
        }
        tracing::info!(
            %target,
            cols,
            rows,
            flags,
            pid = std::process::id(),
            "vt channel armed (pipe-pane <-> vt100 grid)"
        );

        Some(Self {
            name: name.to_string(),
            input,
            owner_id: owner,
            target,
            parser,
            stream,
            app_cursor,
            lifecycle,
            wakeup,
            clipboard,
            links,
            chunk_seq,
            last_chunk_ms,
            prev_gap_ms,
            grid_gen,
            sample_cache: Mutex::new(None),
            signals,
            armed_at: Instant::now(),
            sock_dir,
            sock_path,
            settled_chunk_seq: settled_chunk_seq.clone(),
            snapshot: snapshot.clone(),
            drain: drain.clone(),
            stop,
            reader: Mutex::new(Some(reader)),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            last_size_check: Mutex::new(Instant::now()),
            pending_drift: Mutex::new(None),
            last_owner_hb: Mutex::new(Instant::now()),
            resize: Mutex::new(ResizeState::default()),
        })
    }

    /// Refresh the VT-owner heartbeat while someone samples, rate-limited below
    /// `VT_OWNER_TTL`.
    fn refresh_owner_heartbeat(&self, deadline: &crate::tmux::TmuxCommandDeadline) {
        let mut guard = self.last_owner_hb.lock().unwrap();
        if guard.elapsed() < Duration::from_millis(1500) {
            return;
        }
        *guard = Instant::now();
        drop(guard);
        let _ = crate::tmux::Session::from_name(&self.name)
            .refresh_vt_owner_with_deadline(&self.owner_id, deadline);
    }

    /// Reconcile the grid with the pane at most once a second, reseeding on a
    /// geometry change or a confirmed cursor drift (see [`reconcile_step`]).
    fn reconcile_grid(&self, deadline: &crate::tmux::TmuxCommandDeadline) {
        let mut guard = self.last_size_check.lock().unwrap();
        if guard.elapsed() < Duration::from_secs(1) {
            return;
        }
        *guard = Instant::now();
        drop(guard);
        let probe = self.resize_observation();
        let Some((c, r, cx, cy)) = pane_size_cursor(&self.target, deadline) else {
            return;
        };
        let (gc, gr) = (
            self.cols.load(Ordering::Relaxed),
            self.rows.load(Ordering::Relaxed),
        );
        // Cursor and generation under one parser lock, so a processed chunk's cursor
        // never pairs with an older generation.
        let Ok(p) = self.parser.lock() else {
            return;
        };
        let (gcy, gcx) = p.screen().cursor_position();
        let grid_gen = self.grid_gen.load(Ordering::Relaxed);
        drop(p);
        self.observe_pane_geometry((c, r), probe);
        let pending = self.pending_drift.lock().ok().and_then(|guard| *guard);
        match reconcile_step((c, r, cx, cy), (gc, gr, gcx, gcy), pending, grid_gen) {
            GridReconcile::InSync => self.clear_drift(),
            GridReconcile::ArmDrift => {
                if let Ok(mut guard) = self.pending_drift.lock() {
                    *guard = Some(grid_gen);
                }
            }
            GridReconcile::Resize => {
                if refresh_commits_geometry(self.reseed(c, r, false, deadline)) {
                    self.cols.store(c, Ordering::Relaxed);
                    self.rows.store(r, Ordering::Relaxed);
                }
            }
            GridReconcile::Reseed => {
                tracing::debug!(
                    target: "tmux.vt",
                    pane = %self.target,
                    tmux_cursor = ?(cx, cy),
                    grid_cursor = ?(gcx, gcy),
                    "vt: grid diverged from pane; reseeding",
                );
                self.reseed(c, r, true, deadline);
            }
        }
    }

    fn clear_drift(&self) {
        if let Ok(mut guard) = self.pending_drift.lock() {
            *guard = None;
        }
    }

    /// Reseed from `capture-pane`. Healing reseeds are `guarded` (the current grid
    /// owns concurrent output); resize reseeds are not.
    fn reseed(
        &self,
        cols: u16,
        rows: u16,
        guarded: bool,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> VtRefreshResult {
        // Resize reseeds retry Busy: tmux's full repaint is almost always in flight.
        let attempts = if guarded { 1 } else { SEED_INSTALL_ATTEMPTS };
        let mut result = VtRefreshResult::Failed;
        for attempt in 0..attempts {
            if attempt > 0 {
                std::thread::sleep(SEED_INSTALL_RETRY);
            }
            result = seed_parser(
                &self.target,
                SeedSink {
                    parser: &self.parser,
                    app_cursor: &self.app_cursor,
                    grid_gen: &self.grid_gen,
                    links: &self.links,
                },
                guarded,
                (cols, rows),
                deadline,
                Some((&self.chunk_seq, &self.settled_chunk_seq)),
                SeedInstallFence {
                    snapshot: Some(&self.snapshot),
                    socket: Some(&self.stream),
                    control: Some(&self.drain),
                },
            );
            if result != VtRefreshResult::Busy {
                break;
            }
        }
        if result == VtRefreshResult::Refreshed {
            self.clear_drift();
        }
        result
    }

    /// Reseed even when probes agree, healing cell drift they cannot see.
    pub(crate) fn refresh_authoritatively(
        &self,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> VtRefreshResult {
        self.reseed(
            self.cols.load(Ordering::Relaxed),
            self.rows.load(Ordering::Relaxed),
            true,
            deadline,
        )
    }
    /// Up to `max_lines` of scrollback plus screen as ANSI, with the grid cursor.
    #[cfg(test)]
    pub(crate) fn sample(&self, max_lines: usize) -> VtSample {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.sample_with_deadline(max_lines, &deadline)
    }

    pub(crate) fn sample_with_deadline(
        &self,
        max_lines: usize,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> VtSample {
        self.sample_with_clock(max_lines, deadline, chunk_now_ms)
    }

    fn sample_with_clock(
        &self,
        max_lines: usize,
        deadline: &crate::tmux::TmuxCommandDeadline,
        clock: impl Fn() -> u64,
    ) -> VtSample {
        self.reconcile_grid(deadline);
        self.refresh_owner_heartbeat(deadline);
        let cols = self.cols.load(Ordering::Relaxed);
        let rows = self.rows.load(Ordering::Relaxed);
        let mut p = match self.parser.lock() {
            Ok(p) => p,
            Err(_) => return VtSample::whole(String::new(), None),
        };
        // Under the parser lock, where the reader applies chunks and releases brackets.
        let grid_gen = self.grid_gen.load(Ordering::Relaxed);
        let incomplete = self.signals.incomplete_within(clock());
        if let Ok(guard) = self.sample_cache.lock() {
            if let Some(c) = guard.as_ref() {
                let same_window = (c.max_lines, c.cols, c.rows) == (max_lines, cols, rows);
                // Mid-bracket: serve the last complete frame.
                if same_window && (c.grid_gen == grid_gen || incomplete) {
                    return VtSample::whole(c.content.clone(), Some(c.cursor));
                }
            }
        }
        let (content, history) = grid_content(&mut p, max_lines, cols, rows);
        let mut cursor = cursor_from_screen(p.screen(), rows, cols);
        cursor.history_size = history as u32;
        drop(p);
        // Never cache a half-drawn frame.
        if !incomplete {
            if let Ok(mut guard) = self.sample_cache.lock() {
                *guard = Some(SampleCache {
                    grid_gen,
                    max_lines,
                    cols,
                    rows,
                    content: content.clone(),
                    cursor,
                });
            }
        }
        VtSample {
            content,
            cursor: Some(cursor),
            incomplete,
        }
    }

    /// The visible grid as `want_rows` rows padded to `want_cols`, for compositing.
    /// Padding and truncating keeps a mid-resize disagreement merely stale.
    pub(crate) fn sample_rows_padded_with_deadline(
        &self,
        want_cols: u16,
        want_rows: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<VtRowsSample> {
        self.sample_rows_padded_with_clock(want_cols, want_rows, deadline, chunk_now_ms)
    }

    fn sample_rows_padded_with_clock(
        &self,
        want_cols: u16,
        want_rows: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
        clock: impl Fn() -> u64,
    ) -> Option<VtRowsSample> {
        self.reconcile_grid(deadline);
        self.refresh_owner_heartbeat(deadline);
        let cols = self.cols.load(Ordering::Relaxed);
        let rows = self.rows.load(Ordering::Relaxed);
        let want_cols = want_cols.max(1);
        let want_rows = want_rows.max(1);

        let p = self.parser.lock().ok()?;
        let incomplete = self.signals.incomplete_within(clock());
        let screen = p.screen();
        let readable_cols = cols.min(want_cols);
        let out = (0..want_rows)
            .map(|row| {
                if row >= rows {
                    return " ".repeat(want_cols as usize);
                }
                let last = row_last_col(screen, row, readable_cols);
                let mut line = row_to_ansi_upto(screen, row, last);
                if last < want_cols {
                    line.push_str("\x1b[0m");
                    line.extend(std::iter::repeat_n(' ', (want_cols - last) as usize));
                }
                line
            })
            .collect();
        let cursor = cursor_from_screen(screen, rows, cols);
        drop(p);
        Some(VtRowsSample {
            rows: out,
            cursor,
            incomplete,
        })
    }

    /// Fires on publishable grid changes, OSC 52 writes and death; one per viewer.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.signals.changed_tx.subscribe()
    }

    /// Start a clipboard consumer that skips earlier writes.
    pub(crate) fn clipboard_sequence(&self) -> u64 {
        self.signals.clipboard_seq.load(Ordering::Acquire)
    }

    /// The latest OSC 52 write after `seen`; non-consuming.
    pub(crate) fn clipboard_after(&self, seen: &mut u64) -> Option<String> {
        osc52_clipboard_after(
            &self.signals.clipboard_latest,
            &self.signals.clipboard_seq,
            seen,
        )
    }

    /// Re-sync to a new pane size right after the owner's `resize-window`.
    pub(crate) fn set_grid_size_with_deadline(
        &self,
        cols: u16,
        rows: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> VtRefreshResult {
        if cols == 0 || rows == 0 {
            return VtRefreshResult::Failed;
        }
        if (cols, rows)
            == (
                self.cols.load(Ordering::Relaxed),
                self.rows.load(Ordering::Relaxed),
            )
        {
            return VtRefreshResult::Refreshed;
        }
        self.expect_grid_size(cols, rows);
        let result = self.reseed(cols, rows, false, deadline);
        if refresh_commits_geometry(result) {
            self.cols.store(cols, Ordering::Relaxed);
            self.rows.store(rows, Ordering::Relaxed);
            self.signals.bump_changed();
        }
        result
    }

    fn resize_state(&self) -> std::sync::MutexGuard<'_, ResizeState> {
        self.resize.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Declare the geometry the pane is being resized to; viewers stay off the
    /// grid until the parser is rebuilt at it.
    fn expect_grid_size(&self, cols: u16, rows: u16) {
        self.resize_state().declare(pack_size(cols, rows));
    }

    /// Declare the target geometry and mark a resize in flight until the guard
    /// drops. Every pane resize must go through this.
    pub(crate) fn begin_resize(&self, cols: u16, rows: u16) -> ResizeInFlight<'_> {
        let token = self.resize_state().begin(pack_size(cols, rows));
        ResizeInFlight {
            channel: self,
            token,
            withdrawn: false,
        }
    }

    fn resize_observation(&self) -> ResizeObservation {
        ResizeObservation {
            epoch: self.resize_state().epoch,
        }
    }

    /// Resolve an outstanding expectation against tmux's reported geometry: a match
    /// retires it (only if nothing moved since `probe`), a divergence re-aims it.
    /// Never opens one.
    fn observe_pane_geometry(&self, pane: (u16, u16), probe: ResizeObservation) {
        let mut state = self.resize_state();
        if state.owed == 0 {
            return;
        }
        if pane
            != (
                self.cols.load(Ordering::Relaxed),
                self.rows.load(Ordering::Relaxed),
            )
        {
            state.declare(pack_size(pane.0, pane.1));
            return;
        }
        if state.settled_since(probe) {
            state.owed = 0;
        }
    }

    /// The grid still has the pre-resize layout, so viewers use `capture-pane`.
    pub(crate) fn grid_resync_pending(&self) -> bool {
        self.pending_resync_target().is_some()
    }

    pub(crate) fn pending_resync_target(&self) -> Option<(u16, u16)> {
        let mut state = self.resize_state();
        if state.owed == 0 {
            return None;
        }
        if state.owed
            == pack_size(
                self.cols.load(Ordering::Relaxed),
                self.rows.load(Ordering::Relaxed),
            )
        {
            // Read and cleared under one lock so a newer declaration is not retired.
            state.owed = 0;
            return None;
        }
        Some(((state.owed >> 16) as u16, state.owed as u16))
    }

    /// Reconcile from a non-sampling caller, so a pending expectation can resolve
    /// while the grid is out of service.
    pub(crate) fn reconcile_with_deadline(&self, deadline: &crate::tmux::TmuxCommandDeadline) {
        self.reconcile_grid(deadline);
    }

    pub(crate) fn seed_age(&self) -> Duration {
        self.armed_at.elapsed()
    }

    pub(crate) fn sync_hold_active(&self) -> bool {
        self.signals.hold_active()
    }

    /// Forwarder connected and reader running.
    pub(crate) fn is_alive(&self) -> bool {
        self.lifecycle() == VtLifecycle::Live
    }

    pub(crate) fn lifecycle(&self) -> VtLifecycle {
        VtLifecycle::load(&self.lifecycle)
    }

    /// Take the newest OSC 52 write (consuming, single slot; one consumer).
    pub(crate) fn take_clipboard(&self) -> Option<String> {
        self.clipboard
            .lock()
            .ok()
            .and_then(|mut guard| guard.take())
    }

    /// Register the in-process poller wakeup poked on grid change and death.
    pub(crate) fn set_change_wakeup(&self, wakeup: ChangeWakeup) {
        if let Ok(mut guard) = self.wakeup.lock() {
            *guard = Some(wakeup);
        }
    }

    /// `(since_last_chunk_ms, prev_gap_ms)` for the capture debounce; `None` before
    /// the first chunk.
    pub(crate) fn chunk_timing(&self) -> Option<(u64, u64)> {
        if self.chunk_seq.load(Ordering::Relaxed) == 0 {
            return None;
        }
        let since_last = chunk_now_ms().saturating_sub(self.last_chunk_ms.load(Ordering::Relaxed));
        Some((since_last, self.prev_gap_ms.load(Ordering::Relaxed)))
    }

    fn write_input(&self, bytes: &[u8]) -> bool {
        use std::io::Write;
        if !self.input {
            return false;
        }
        let mut guard = self.stream.lock().unwrap();
        match guard.as_mut() {
            Some(stream) => stream.write_all(bytes).is_ok(),
            None => false,
        }
    }
    pub(crate) fn shutdown_with_deadline(&self, deadline: &crate::tmux::TmuxCommandDeadline) {
        if self.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        crate::tmux::Session::from_name(&self.name)
            .release_vt_pipe_owner_with_deadline(&self.owner_id, deadline);
        let _ = UnixStream::connect(&self.sock_path);
        let _ = UnixStream::connect(self.sock_dir.join("c.sock"));
        if let Some(reader) = self.reader.lock().unwrap().take() {
            let _ = reader.join();
        }
        let _ = std::fs::remove_dir_all(&self.sock_dir);
    }
}
impl Drop for VtChannel {
    fn drop(&mut self) {
        {
            let mut registry = REGISTRY.lock().unwrap();
            if registry
                .get(&self.name)
                .is_some_and(|channel| channel.upgrade().is_none())
            {
                registry.remove(&self.name);
            }
        }
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.shutdown_with_deadline(&deadline);
    }
}

/// A raw `pipe-pane` reader that only observes OSC 52 writes, for shell
/// previews rendered through `capture-pane`.
pub(crate) struct Osc52Channel {
    name: String,
    owner_id: String,
    clipboard: Arc<Mutex<Option<String>>>,
    /// Bumped per clipboard write; consumers keep their own cursor.
    clipboard_seq: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    sock_dir: PathBuf,
    sock_path: PathBuf,
    last_owner_hb: Mutex<Instant>,
}

impl Osc52Channel {
    /// Arm a read-only observer under the same cross-process owner lease.
    pub(crate) fn acquire(name: &str) -> Option<Arc<Self>> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        Self::acquire_with_deadline(name, &deadline)
    }

    pub(crate) fn acquire_with_deadline(
        name: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<Arc<Self>> {
        if let Some(channel) = lookup_osc52(name).filter(|channel| channel.is_alive()) {
            return Some(channel);
        }
        let arm_lock = OSC52_ARM_LOCKS
            .lock()
            .unwrap()
            .entry(name.to_string())
            .or_default()
            .clone();
        let result = {
            let _armed = arm_lock.lock().unwrap();
            if let Some(channel) = lookup_osc52(name).filter(|channel| channel.is_alive()) {
                Some(channel)
            } else {
                Self::arm(name, deadline).map(|channel| {
                    let channel = Arc::new(channel);
                    OSC52_REGISTRY
                        .lock()
                        .unwrap()
                        .insert(name.to_string(), Arc::downgrade(&channel));
                    channel
                })
            }
        };
        drop(arm_lock);
        OSC52_ARM_LOCKS
            .lock()
            .unwrap()
            .retain(|_, lock| Arc::strong_count(lock) > 1);
        result
    }

    fn arm(name: &str, deadline: &crate::tmux::TmuxCommandDeadline) -> Option<Self> {
        if !tmux_supports_pipe_pane_io(deadline) {
            return None;
        }
        let session = crate::tmux::Session::from_name(name);
        let owner = new_pipe_owner_id();
        if !session.claim_vt_owner_with_deadline(
            &owner,
            crate::tmux::session::VT_OWNER_TTL,
            deadline,
        ) {
            return None;
        }

        let n = SOCK_COUNTER.fetch_add(1, Ordering::Relaxed);
        let sock_dir = std::env::temp_dir().join(format!("aoe-osc52-{}-{n}", std::process::id()));
        let setup = || -> Option<(PathBuf, UnixListener)> {
            std::fs::create_dir_all(&sock_dir).ok()?;
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&sock_dir, std::fs::Permissions::from_mode(0o700)).ok()?;
            }
            let sock_path = sock_dir.join("s.sock");
            Some((sock_path.clone(), UnixListener::bind(sock_path).ok()?))
        };
        let Some((sock_path, listener)) = setup() else {
            let _ = std::fs::remove_dir_all(&sock_dir);
            session.release_vt_owner_with_deadline(&owner, deadline);
            return None;
        };
        let Some(exe) = std::env::current_exe().ok() else {
            let _ = std::fs::remove_dir_all(&sock_dir);
            session.release_vt_owner_with_deadline(&owner, deadline);
            return None;
        };

        let alive = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let clipboard = Arc::new(Mutex::new(None));
        let clipboard_seq = Arc::new(AtomicU64::new(0));
        let reader = {
            let alive = alive.clone();
            let stop = stop.clone();
            let clipboard = clipboard.clone();
            let clipboard_seq = clipboard_seq.clone();
            std::thread::spawn(move || {
                run_osc52_reader(listener, stop, alive, clipboard, clipboard_seq)
            })
        };
        let pipe_cmd = format!(
            "{} __vt-pipe {}",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&sock_path.to_string_lossy())
        );
        let armed = session.arm_vt_pipe_if_owner_with_deadline(&owner, "-O", &pipe_cmd, deadline);
        if !armed {
            stop.store(true, Ordering::Relaxed);
            session.release_vt_pipe_owner_with_deadline(&owner, deadline);
            let _ = UnixStream::connect(&sock_path);
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&sock_dir);
            return None;
        }
        let connect_deadline = Instant::now() + Duration::from_millis(500);
        while !alive.load(Ordering::Relaxed) {
            if Instant::now() >= connect_deadline {
                stop.store(true, Ordering::Relaxed);
                session.release_vt_pipe_owner_with_deadline(&owner, deadline);
                let _ = UnixStream::connect(&sock_path);
                let _ = reader.join();
                let _ = std::fs::remove_dir_all(&sock_dir);
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Some(Self {
            name: name.to_string(),
            owner_id: owner,
            clipboard,
            clipboard_seq,
            alive,
            stop,
            reader: Mutex::new(Some(reader)),
            sock_dir,
            sock_path,
            last_owner_hb: Mutex::new(Instant::now()),
        })
    }

    pub(crate) fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// Start a consumer that skips earlier writes.
    pub(crate) fn clipboard_sequence(&self) -> u64 {
        self.clipboard_seq.load(Ordering::Acquire)
    }

    /// The latest clipboard write after `seen`; non-consuming.
    pub(crate) fn clipboard_after(&self, seen: &mut u64) -> Option<String> {
        osc52_clipboard_after(&self.clipboard, &self.clipboard_seq, seen)
    }

    pub(crate) fn refresh_owner_heartbeat(&self) {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.refresh_owner_heartbeat_with_deadline(&deadline);
    }

    pub(crate) fn refresh_owner_heartbeat_with_deadline(
        &self,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) {
        let Ok(mut last) = self.last_owner_hb.lock() else {
            return;
        };
        if last.elapsed() < Duration::from_millis(1500) {
            return;
        }
        *last = Instant::now();
        drop(last);
        let _ = crate::tmux::Session::from_name(&self.name)
            .refresh_vt_owner_with_deadline(&self.owner_id, deadline);
    }
    pub(crate) fn shutdown_with_deadline(&self, deadline: &crate::tmux::TmuxCommandDeadline) {
        if self.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        crate::tmux::Session::from_name(&self.name)
            .release_vt_pipe_owner_with_deadline(&self.owner_id, deadline);
        let _ = UnixStream::connect(&self.sock_path);
        if let Some(reader) = self.reader.lock().unwrap().take() {
            let _ = reader.join();
        }
        let _ = std::fs::remove_dir_all(&self.sock_dir);
    }
}

fn osc52_clipboard_after(
    clipboard: &Mutex<Option<String>>,
    clipboard_seq: &AtomicU64,
    seen: &mut u64,
) -> Option<String> {
    let seq = clipboard_seq.load(Ordering::Acquire);
    if seq == *seen {
        return None;
    }
    let text = clipboard.lock().ok().and_then(|slot| slot.clone())?;
    *seen = seq;
    Some(text)
}

fn run_osc52_reader(
    listener: UnixListener,
    stop: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    clipboard: Arc<Mutex<Option<String>>>,
    clipboard_seq: Arc<AtomicU64>,
) {
    let Ok((mut conn, _)) = listener.accept() else {
        return;
    };
    alive.store(true, Ordering::Relaxed);
    let _ = conn.set_read_timeout(Some(Duration::from_millis(200)));
    let mut scanner = Osc52Scanner::new();
    let mut buf = [0u8; 8192];
    while !stop.load(Ordering::Relaxed) {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if let Some(text) = scanner.feed(&buf[..n]) {
                    if let Ok(mut slot) = clipboard.lock() {
                        *slot = Some(text);
                        clipboard_seq.fetch_add(1, Ordering::Release);
                    }
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    alive.store(false, Ordering::Relaxed);
}

impl Drop for Osc52Channel {
    fn drop(&mut self) {
        {
            let mut registry = OSC52_REGISTRY.lock().unwrap();
            if registry
                .get(&self.name)
                .is_some_and(|channel| channel.upgrade().is_none())
            {
                registry.remove(&self.name);
            }
        }
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.shutdown_with_deadline(&deadline);
    }
}

/// Test double for a channel that never armed a pipe.
#[cfg(test)]
pub(crate) fn dummy_channel_with_input(
    name: &str,
    dir: &std::path::Path,
    input: bool,
) -> (Arc<VtChannel>, Arc<AtomicU8>) {
    let lifecycle = Arc::new(AtomicU8::new(VtLifecycle::Starting as u8));
    let ch = Arc::new(VtChannel {
        name: name.to_string(),
        input,
        owner_id: new_pipe_owner_id(),
        target: format!("{name}:^.0"),
        parser: Arc::new(Mutex::new(vt100::Parser::new(4, 20, SCROLLBACK_LINES))),
        stream: Arc::new(Mutex::new(None)),
        app_cursor: Arc::new(AtomicBool::new(false)),
        lifecycle: lifecycle.clone(),
        wakeup: Arc::new(Mutex::new(None)),
        clipboard: Arc::new(Mutex::new(None)),
        links: Arc::new(LinkTable::default()),
        chunk_seq: Arc::new(AtomicU64::new(0)),
        settled_chunk_seq: Arc::new(AtomicU64::new(0)),
        snapshot: Arc::new(Mutex::new(())),
        drain: Arc::new(Mutex::new(DrainControl::default())),
        last_chunk_ms: Arc::new(AtomicU64::new(0)),
        prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
        grid_gen: Arc::new(AtomicU64::new(0)),
        signals: Arc::new(ViewerSignals::new()),
        armed_at: Instant::now(),
        sample_cache: Mutex::new(None),
        sock_dir: dir.to_path_buf(),
        sock_path: dir.join("s.sock"),
        stop: Arc::new(AtomicBool::new(false)),
        reader: Mutex::new(None),
        cols: AtomicU16::new(20),
        rows: AtomicU16::new(4),
        last_size_check: Mutex::new(Instant::now()),
        pending_drift: Mutex::new(None),
        last_owner_hb: Mutex::new(Instant::now()),
        resize: Mutex::new(ResizeState::default()),
    });
    (ch, lifecycle)
}

/// Publish a live test double for `name`, as `acquire` would.
#[cfg(test)]
pub(crate) fn register_live_for_test(
    name: &str,
    dir: &std::path::Path,
    input: bool,
    app_cursor: bool,
) -> Arc<VtChannel> {
    let (channel, lifecycle) = dummy_channel_with_input(name, dir, input);
    channel.app_cursor.store(app_cursor, Ordering::Relaxed);
    VtLifecycle::Live.store(&lifecycle);
    REGISTRY
        .lock()
        .unwrap()
        .insert(name.to_string(), Arc::downgrade(&channel));
    channel
}

#[cfg(test)]
pub(crate) struct HeldVtDrain {
    drain: Arc<Mutex<DrainControl>>,
    original: Option<DrainControl>,
    peer: UnixStream,
    probes: std::sync::mpsc::Receiver<()>,
    reader: Option<std::thread::JoinHandle<std::io::Result<()>>>,
}

#[cfg(test)]
impl HeldVtDrain {
    pub(crate) fn observed_probe(&mut self) -> bool {
        self.probes.recv_timeout(Duration::from_secs(5)).is_ok()
    }

    pub(crate) fn acknowledge_next(&mut self) {
        use std::io::Write;
        let mut control = self.drain.lock().unwrap();
        control.next_now = Some(Instant::now());
        let queued = self
            .peer
            .write_all(&drain_frame(DRAIN_ACK, control.next_generation));
        drop(control);
        queued.expect("queue next native drain ACK before its deadline");
    }
}

#[cfg(test)]
impl Drop for HeldVtDrain {
    fn drop(&mut self) {
        let shutdown = self.peer.shutdown(std::net::Shutdown::Both);
        let joined = self.reader.take().unwrap().join();
        *self.drain.lock().unwrap() = self.original.take().unwrap();
        if !std::thread::panicking() {
            shutdown.expect("shut down held drain socket");
            joined
                .expect("held drain reader exits")
                .expect("read native drain probes");
        }
    }
}

#[cfg(test)]
impl VtChannel {
    pub(crate) fn hold_drain_for_test(&self) -> HeldVtDrain {
        let (stream, peer) = UnixStream::pair().expect("held native drain socket");
        peer.set_write_timeout(Some(Duration::from_secs(5)))
            .expect("held drain ACK timeout");
        let mut input = peer.try_clone().expect("held drain reader socket");
        let (observed, probes) = std::sync::mpsc::channel();
        // Withhold ACKs, not reads, so control-socket backpressure is not tested.
        let reader = std::thread::spawn(move || loop {
            match read_drain_frame(&mut input) {
                Ok((DRAIN_PROBE, _)) => {
                    if observed.send(()).is_err() {
                        return Ok(());
                    }
                }
                Ok(frame) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("unexpected held drain frame: {frame:?}"),
                    ));
                }
                Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(err) => return Err(err),
            }
        });
        let original = std::mem::replace(
            &mut *self.drain.lock().unwrap(),
            DrainControl {
                stream: Some(stream),
                ..DrainControl::default()
            },
        );
        HeldVtDrain {
            drain: self.drain.clone(),
            original: Some(original),
            peer,
            probes,
            reader: Some(reader),
        }
    }
}

#[cfg(test)]
pub(crate) fn unregister_for_test(name: &str) {
    REGISTRY.lock().unwrap().remove(name);
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ReaderCtx {
        /// Every field a reader needs, so a test names only what it drives.
        fn for_test() -> Self {
            ReaderCtx {
                snapshot_contended: None,
                parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0))),
                stop: Arc::new(AtomicBool::new(false)),
                seeded: Arc::new(AtomicBool::new(true)),
                snapshot: Arc::new(Mutex::new(())),
                stream: Arc::new(Mutex::new(None)),
                app_cursor: Arc::new(AtomicBool::new(false)),
                lifecycle: Arc::new(AtomicU8::new(VtLifecycle::Starting as u8)),
                wakeup: Arc::new(Mutex::new(None)),
                clipboard: Arc::new(Mutex::new(None)),
                links: Arc::new(LinkTable::default()),
                chunk_seq: Arc::new(AtomicU64::new(0)),
                settled_chunk_seq: Arc::new(AtomicU64::new(0)),
                last_chunk_ms: Arc::new(AtomicU64::new(0)),
                prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
                grid_gen: Arc::new(AtomicU64::new(0)),
                signals: Arc::new(ViewerSignals::new()),
            }
        }
    }

    #[test]
    fn pipe_owner_ids_are_unique_per_channel_generation() {
        assert_ne!(new_pipe_owner_id(), new_pipe_owner_id());
    }

    #[test]
    fn transient_version_failure_is_not_cached() {
        let cache = std::sync::OnceLock::new();
        assert!(!cached_tmux_support(&cache, || None));
        assert!(cache.get().is_none());
        assert!(cached_tmux_support(&cache, || parse_tmux_pipe_support(
            "tmux 3.4"
        )));
        assert_eq!(cache.get(), Some(&true));
        assert!(cached_tmux_support(&cache, || panic!(
            "cached result must win"
        )));

        let cases = [
            ("tmux 3.3a", Some(false)),
            ("tmux next-3.5", Some(true)),
            ("bad", None),
        ];
        for (version, expected) in cases {
            assert_eq!(parse_tmux_pipe_support(version), expected, "{version}");
        }
    }

    #[test]
    fn pipe_input_requires_a_tmux_that_survives_a_dead_pane_write() {
        let cases = [
            ("tmux 3.4", Some(false)),
            ("tmux 3.5a", Some(false)),
            ("tmux 3.7a", Some(false)),
            ("tmux 3.8", Some(true)),
            ("tmux next-3.8", Some(true)),
            ("tmux 4.0", Some(true)),
            ("bad", None),
        ];
        for (version, expected) in cases {
            assert_eq!(
                parse_tmux_pipe_input_support(version),
                expected,
                "{version}"
            );
        }
    }

    #[test]
    fn grid_content_preserves_interior_padding() {
        let mut p = vt100::Parser::new(2, 20, 0);
        p.process(b"A\x1b[12GB");
        let (content, _) = grid_content(&mut p, 2, 20, 2);
        assert!(
            content.contains("A          B"),
            "interior padding collapsed:\n{content:?}"
        );
        assert!(
            !content.contains("\x1b[10C") && !content.contains("\x1b[C"),
            "cursor-forward escape leaked:\n{content:?}"
        );
    }

    fn visible_width(row: &str) -> usize {
        use unicode_width::UnicodeWidthStr;
        UnicodeWidthStr::width(crate::tmux::utils::strip_ansi(row).as_str())
    }

    #[test]
    fn capture_rows_padded_fills_every_row_to_the_pane_width() {
        let rows = capture_rows_padded(b"ab\nlonger\n", 8, 3);
        assert_eq!(rows.len(), 3, "one entry per pane row, blanks included");
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(visible_width(row), 8, "row {i} not padded: {row:?}");
        }
        assert!(rows[0].contains("ab"));
        assert!(rows[1].contains("longer"));
    }

    #[test]
    fn capture_rows_padded_unstaircases_bare_lf_input() {
        let rows = capture_rows_padded(b"line-1\nline-2\n", 10, 2);
        let plain: Vec<String> = rows
            .iter()
            .map(|r| crate::tmux::utils::strip_ansi(r))
            .collect();
        assert_eq!(plain[0].trim_end(), "line-1");
        assert_eq!(plain[1].trim_end(), "line-2", "row 1 staircased");
    }

    #[test]
    fn capture_rows_padded_resets_style_before_padding() {
        let rows = capture_rows_padded(b"\x1b[41mred", 8, 1);
        assert_eq!(visible_width(&rows[0]), 8);
        assert!(
            rows[0].ends_with("\x1b[0m     "),
            "padding not reset: {:?}",
            rows[0]
        );
    }

    #[test]
    fn capture_rows_padded_counts_a_trailing_wide_glyph_as_two_columns() {
        let rows = capture_rows_padded("ab漢".as_bytes(), 4, 1);
        assert_eq!(
            visible_width(&rows[0]),
            4,
            "row should exactly fill the pane: {:?}",
            rows[0]
        );
        assert!(
            !rows[0].ends_with(' '),
            "no padding belongs on a row that already fills its width: {:?}",
            rows[0]
        );

        let rows = capture_rows_padded("ab漢".as_bytes(), 7, 1);
        assert_eq!(visible_width(&rows[0]), 7, "{:?}", rows[0]);

        let rows = capture_rows_padded("abc漢".as_bytes(), 4, 2);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(visible_width(r), 4, "row {i}: {r:?}");
        }
    }

    #[test]
    fn capture_rows_padded_survives_a_one_row_pane_that_wraps() {
        let rows = capture_rows_padded(b"keep", 3, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(visible_width(&rows[0]), 3);
    }

    #[test]
    fn capture_rows_padded_truncates_content_wider_than_the_pane() {
        let rows = capture_rows_padded(b"abcdefgh", 4, 2);
        assert_eq!(visible_width(&rows[0]), 4);
        assert_eq!(visible_width(&rows[1]), 4);
    }

    #[test]
    fn seed_install_reports_failure_and_preserves_newer_chunks() {
        let parser = Mutex::new(vt100::Parser::new(24, 80, SCROLLBACK_LINES));
        parser.lock().unwrap().process(b"LIVE-CHUNK");
        let app_cursor = AtomicBool::new(false);
        let grid_gen = AtomicU64::new(0);
        let chunk_seq = AtomicU64::new(1);
        let settled_chunk_seq = AtomicU64::new(0);

        assert_eq!(
            swap_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                b"STALE-SNAPSHOT",
                (80, 24),
                SeedGuard {
                    chunk: Some((&chunk_seq, &settled_chunk_seq, 0)),
                    pipe: None,
                },
            ),
            VtRefreshResult::Busy,
        );
        assert_eq!(
            swap_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                b"STALE-SNAPSHOT",
                (80, 24),
                SeedGuard {
                    chunk: Some((&chunk_seq, &settled_chunk_seq, 1)),
                    pipe: None,
                },
            ),
            VtRefreshResult::Busy,
            "a seed must not overtake a read waiting on the parser"
        );
        let contents = parser.lock().unwrap().screen().contents();
        assert!(contents.contains("LIVE-CHUNK"));
        assert!(!contents.contains("STALE-SNAPSHOT"));

        let deadline = crate::tmux::TmuxCommandDeadline::with_timeout(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(
            seed_parser(
                "aoe_test_missing_seed",
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                false,
                (80, 24),
                &deadline,
                None,
                SeedInstallFence {
                    snapshot: None,
                    socket: None,
                    control: None,
                },
            ),
            VtRefreshResult::Failed,
        );
    }
    #[test]
    fn lf_to_crlf_unstaircases_seed_rows() {
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
        assert_eq!(
            strip_trailing_row_terminator(b"line-1\nREADY> \n\n\n"),
            b"line-1\nREADY> \n\n"
        );
        assert_eq!(strip_trailing_row_terminator(b"a\r\nb\r\n"), b"a\r\nb");
        assert_eq!(strip_trailing_row_terminator(b"READY>"), b"READY>");
        assert_eq!(strip_trailing_row_terminator(b""), b"");
    }

    #[test]
    fn seed_places_cursor_at_queried_position_not_end_of_content() {
        let rows: u16 = 6;
        let cols: u16 = 20;
        let body = b"row0-full-content\nrow1-full-content\nrow2-full-content\nrow3-full-content\nrow4-full-content\nrow5-full-content\n";
        let state = PaneSeedState {
            cursor_x: 3,
            cursor_y: 1,
            cursor_visible: true,
            pane_height: rows,
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
        assert!(
            p.screen().contents().contains("row0-full-content"),
            "top row must survive (no over-scroll):\n{}",
            p.screen().contents()
        );
    }

    #[test]
    fn seed_hides_cursor_when_pane_hid_it() {
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
        let rows: u16 = 4;
        let cols: u16 = 12;
        let mut body = Vec::new();
        for i in 0..10 {
            body.extend_from_slice(format!("HL{i:02}\n").as_bytes());
        }
        let state = PaneSeedState {
            cursor_x: 2,
            cursor_y: 1,
            cursor_visible: true,
            pane_height: rows,
            ..Default::default()
        };
        let mut p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        p.process(&assemble_seed_stream(&body, &state, rows));
        assert_eq!(
            p.screen().cursor_position(),
            (1, 2),
            "cursor row is visible-screen-relative, not counted from the top of history"
        );
        assert!(
            p.screen().contents().contains("HL09"),
            "newest row must be on the visible screen:\n{}",
            p.screen().contents()
        );
    }

    #[test]
    fn seed_keeps_cursor_on_the_prompt_when_the_pane_outgrows_the_grid() {
        let rows: u16 = 6;
        let cols: u16 = 20;
        let pane_height: u16 = 8;
        let mut body = Vec::new();
        for i in 0..3 {
            body.extend_from_slice(format!("line-{i}\n").as_bytes());
        }
        body.extend_from_slice(b"READY> \n");
        for _ in 4..pane_height {
            body.extend_from_slice(b"\n");
        }
        let state = PaneSeedState {
            cursor_x: 7,
            cursor_y: 3,
            cursor_visible: true,
            pane_height,
            ..Default::default()
        };
        let mut p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
        p.process(&assemble_seed_stream(&body, &state, rows));

        assert_eq!(
            p.screen().cursor_position(),
            (1, 7),
            "cursor must follow the prompt row the taller body pushed up:\n{}",
            p.screen().contents()
        );
        assert!(
            p.screen().contents().contains("READY>"),
            "prompt must be on the visible screen:\n{}",
            p.screen().contents()
        );
    }

    #[test]
    fn seeded_cursor_row_reduces_to_cursor_y_when_heights_agree() {
        let body = |rows: usize| -> Vec<u8> {
            let mut out = Vec::new();
            for i in 0..rows {
                out.extend_from_slice(format!("r{i}\n").as_bytes());
            }
            out
        };
        let cases: [(usize, u16, u16, u16, u16); 4] = [
            (4, 4, 2, 4, 2),
            (10, 4, 2, 4, 2),
            (4, 0, 2, 4, 2),
            (4, 2, 1, 6, 3),
        ];
        for (body_rows, pane_height, cursor_y, rows, want) in cases {
            let state = PaneSeedState {
                cursor_y,
                pane_height,
                ..Default::default()
            };
            assert_eq!(
                seeded_cursor_row(&body(body_rows), &state, rows),
                want,
                "body_rows={body_rows} pane_height={pane_height} cursor_y={cursor_y} rows={rows}"
            );
        }
    }

    #[test]
    fn parse_seed_state_reads_extended_probe_fields() {
        let s = parse_seed_state("1 0 1 0 7 12 0 1 345 48 120");
        assert!(s.alt && !s.mouse && s.mouse_sgr && !s.mouse_all);
        assert_eq!((s.cursor_x, s.cursor_y), (7, 12));
        assert!(!s.cursor_visible && s.app_cursor);
        assert_eq!(
            (s.history_size, s.pane_height, s.pane_width),
            (345, 48, 120)
        );
        let short = parse_seed_state("0 0 0 0 3 4");
        assert_eq!((short.cursor_x, short.cursor_y), (3, 4));
        assert!(short.cursor_visible);
        assert_eq!(
            (short.history_size, short.pane_height, short.pane_width),
            (0, 0, 0)
        );
    }

    #[test]
    fn split_seed_capture_separates_body_and_probe() {
        let raw = b"row-a\n\n\nrow-d\n0 0 0 0 5 3 1 0 12 24\n";
        let (body, probe) = split_seed_capture(raw);
        assert_eq!(body, b"row-a\n\n\nrow-d\n");
        let post = parse_seed_state(probe);
        assert_eq!((post.cursor_x, post.cursor_y), (5, 3));
        assert_eq!((post.history_size, post.pane_height), (12, 24));

        let (body, probe) = split_seed_capture(b"0 0 0 0 1 2 1 0 0 5\n");
        assert!(body.is_empty());
        assert_eq!(parse_seed_state(probe).cursor_y, 2);

        assert_eq!(split_seed_capture(b""), (&b""[..], ""));
    }

    #[test]
    fn is_probe_line_rejects_swallowed_capture_rows() {
        let fields = SEED_STATE_FMT.split_whitespace().count();
        let probe = vec!["7"; fields].join(" ");
        assert!(is_probe_line(&probe));
        assert!(!is_probe_line("$ cargo build --release"));
        assert!(!is_probe_line("zsh: command not found: python"));
        assert!(!is_probe_line(&vec!["1"; fields - 1].join(" ")));
        assert!(!is_probe_line(&vec!["1"; fields + 1].join(" ")));
        assert!(!is_probe_line(""));
    }

    #[test]
    fn seed_probe_agreement_detects_drift() {
        let base = parse_seed_state("0 0 0 0 10 20 1 0 100 40 80");
        assert_eq!(base, parse_seed_state("0 0 0 0 10 20 1 0 100 40 80"));
        assert_ne!(base, parse_seed_state("0 0 0 0 11 20 1 0 100 40 80"));
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 1 0 101 40 80"));
        assert_ne!(base, parse_seed_state("1 0 0 0 10 20 1 0 100 40 80"));
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 1 0 100 41 80"));
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 1 0 100 40 79"));
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 0 0 100 40 80"));
    }

    fn dummy_channel(name: &str, dir: &std::path::Path) -> (Arc<VtChannel>, Arc<AtomicU8>) {
        dummy_channel_with_input(name, dir, true)
    }

    #[test]
    fn output_only_channel_never_writes_to_the_pane() {
        for (input, delivered) in [(false, false), (true, true)] {
            let name = format!("aoe_test_vt_input_{input}_{}", std::process::id());
            let dir = tempfile::tempdir().expect("tempdir");
            let listener = UnixListener::bind(dir.path().join("s.sock")).expect("bind");
            let writer = UnixStream::connect(dir.path().join("s.sock")).expect("connect");
            let (mut pane_side, _) = listener.accept().expect("accept");
            pane_side
                .set_read_timeout(Some(Duration::from_millis(100)))
                .expect("read timeout");
            let (channel, lifecycle) = dummy_channel_with_input(&name, dir.path(), input);
            *channel.stream.lock().unwrap() = Some(writer);
            VtLifecycle::Live.store(&lifecycle);
            REGISTRY
                .lock()
                .unwrap()
                .insert(name.clone(), Arc::downgrade(&channel));

            assert_eq!(input_mode(&name).is_some(), delivered, "input={input}");
            assert_eq!(try_send_input(&name, b"x"), delivered, "input={input}");
            let mut buf = [0u8; 8];
            let got = pane_side.read(&mut buf).unwrap_or(0);
            let want: &[u8] = if delivered { b"x" } else { b"" };
            assert_eq!(&buf[..got], want, "input={input}");

            REGISTRY.lock().unwrap().remove(&name);
        }
    }

    #[test]
    fn output_only_channel_still_reports_the_pane_cursor_mode() {
        let name = format!("aoe_test_vt_cursor_mode_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let channel = register_live_for_test(&name, dir.path(), false, true);

        assert_eq!(cursor_mode(&name), Some(true));
        assert_eq!(input_mode(&name), None);

        VtLifecycle::fail(&channel.lifecycle);
        assert_eq!(cursor_mode(&name), None, "a dead grid's mode is stale");

        unregister_for_test(&name);
    }

    #[test]
    fn authoritative_refresh_reseeds_rather_than_standing_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (channel, lifecycle) = dummy_channel("aoe_test_vt_fallback", dir.path());
        VtLifecycle::Live.store(&lifecycle);
        channel.stop.store(true, Ordering::Relaxed);

        let deadline = crate::tmux::TmuxCommandDeadline::with_timeout(Duration::ZERO);
        assert_eq!(
            channel.refresh_authoritatively(&deadline),
            VtRefreshResult::Failed,
            "a capture that cannot run reports Failed, not a stand-down"
        );
        assert_eq!(
            channel.lifecycle(),
            VtLifecycle::Live,
            "a failed refresh must leave the live grid in service"
        );
    }

    #[test]
    fn failed_registry_entry_is_rearmed_rather_than_reused() {
        let name = format!("aoe_test_vt_failed_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (channel, lifecycle) = dummy_channel(&name, dir.path());
        VtLifecycle::fail(&lifecycle);
        REGISTRY
            .lock()
            .unwrap()
            .insert(name.clone(), Arc::downgrade(&channel));

        assert_eq!(
            lookup(&name).map(|c| c.lifecycle()),
            Some(VtLifecycle::Failed),
        );
        assert!(VtChannel::acquire(&name).is_none());

        REGISTRY.lock().unwrap().remove(&name);
    }

    #[test]
    fn only_a_live_channel_answers_for_pane_links() {
        let name = format!("aoe_test_vt_links_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (channel, lifecycle) = dummy_channel(&name, dir.path());
        record_links(
            &channel.links,
            vec![PaneLink {
                text: "docs".to_string(),
                uri: "https://example.com/old".to_string(),
            }],
        );
        REGISTRY
            .lock()
            .unwrap()
            .insert(name.clone(), Arc::downgrade(&channel));

        VtLifecycle::Live.store(&lifecycle);
        assert_eq!(pane_links(&name).len(), 1, "a live channel still answers");
        let live_generation = pane_links_generation(&name);
        assert_ne!(live_generation, 0, "a recorded link moved the generation");

        {
            let gone = VtLifecycle::Failed;
            VtLifecycle::fail(&lifecycle);
            assert!(
                pane_links(&name).is_empty(),
                "{gone:?} must not serve the table it froze at teardown",
            );
            assert_eq!(
                pane_links_generation(&name),
                0,
                "{gone:?} must drop to the no-channel zero so consumers re-collect",
            );
        }

        REGISTRY.lock().unwrap().remove(&name);
    }

    #[test]
    fn reader_exit_marks_the_lifecycle_failed() {
        let lifecycle = AtomicU8::new(VtLifecycle::Live as u8);
        VtLifecycle::fail(&lifecycle);
        assert_eq!(VtLifecycle::load(&lifecycle), VtLifecycle::Failed);
    }

    #[test]
    fn expired_deadline_bounds_worker_owned_channel_shutdown() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (channel, _) = dummy_channel("aoe_test_vt_shutdown", dir.path());
        let channel = Arc::try_unwrap(channel).ok().expect("sole channel owner");
        let deadline = crate::tmux::TmuxCommandDeadline::with_timeout(Duration::from_millis(1));
        std::thread::sleep(Duration::from_millis(5));
        let started = Instant::now();
        channel.shutdown_with_deadline(&deadline);
        assert!(channel.stop.load(Ordering::Relaxed));
        assert!(started.elapsed() < Duration::from_millis(500));
        drop(channel);

        let late_dir = tempfile::tempdir().expect("late reader tempdir");
        let sock_path = late_dir.path().join("late-reader.sock");
        let listener = UnixListener::bind(&sock_path).expect("bind late reader");
        let stop = Arc::new(AtomicBool::new(false));
        let reader_stop = stop.clone();
        let reader = std::thread::spawn(move || {
            let _ = listener.accept();
            let deadline = Instant::now() + Duration::from_millis(750);
            while !reader_stop.load(Ordering::Relaxed) && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let started = Instant::now();
        stop_and_wake_reader(&stop, &sock_path);
        reader.join().expect("late reader exits");
        assert!(stop.load(Ordering::Relaxed));
        assert!(
            started.elapsed() < Duration::from_millis(250),
            "arm-timeout cleanup must stop a reader accepted after the deadline",
        );
    }

    #[test]
    fn sample_rows_padded_renders_the_visible_grid_at_the_requested_rectangle() {
        let name = format!("aoe_test_vt_padded_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel(&name, dir.path());
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        let sample_rows =
            |cols, rows| ch.sample_rows_padded_with_clock(cols, rows, &deadline, || 100);
        ch.parser
            .lock()
            .unwrap()
            .process(b"hello\r\nworld\r\n\x1b[41mfilled");

        let sample = sample_rows(20, 4).expect("sample");
        let (rows, cursor) = (sample.rows, sample.cursor);
        assert!(!sample.incomplete, "no bracket open: publishable");
        assert_eq!(rows.len(), 4);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(
                crate::tmux::utils::strip_ansi(r).chars().count(),
                20,
                "row {i} not padded to width: {r:?}"
            );
        }
        assert!(crate::tmux::utils::strip_ansi(&rows[0]).starts_with("hello"));
        assert!(crate::tmux::utils::strip_ansi(&rows[1]).starts_with("world"));
        assert!(cursor.position_reliable);

        let rows = sample_rows(6, 2).expect("sample").rows;
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(crate::tmux::utils::strip_ansi(r).chars().count(), 6);
        }

        let rows = sample_rows(10, 6).expect("sample").rows;
        assert_eq!(rows.len(), 6);
        for (i, r) in rows.iter().enumerate() {
            let plain = crate::tmux::utils::strip_ansi(r);
            assert_eq!(plain.chars().count(), 10, "row {i}: {r:?}");
            if i >= 4 {
                assert!(plain.trim().is_empty(), "row {i} should be filler: {r:?}");
            }
        }

        ch.signals.begin_hold(100);
        let held = sample_rows(20, 4).expect("sample");
        assert!(held.incomplete, "mid-bracket rows are not publishable");
        ch.signals.end_hold();
        assert!(!sample_rows(20, 4).expect("sample").incomplete);
    }

    #[test]
    fn acquire_does_not_reuse_a_dead_channel() {
        let name = format!("aoe_test_vt_dead_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (dead, _alive) = dummy_channel(&name, dir.path());
        REGISTRY
            .lock()
            .unwrap()
            .insert(name.clone(), Arc::downgrade(&dead));

        let got = VtChannel::acquire(&name);
        assert!(
            got.is_none_or(|c| c.is_alive()),
            "acquire must never return a dead channel"
        );

        REGISTRY.lock().unwrap().remove(&name);
    }

    #[test]
    fn concurrent_acquire_for_one_session_serializes_without_deadlock() {
        let name = format!("aoe_test_vt_race_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (dead, _alive) = dummy_channel(&name, dir.path());
        REGISTRY
            .lock()
            .unwrap()
            .insert(name.clone(), Arc::downgrade(&dead));

        let arm_lock = Arc::new(Mutex::new(()));
        ARM_LOCKS
            .lock()
            .unwrap()
            .insert(name.clone(), arm_lock.clone());
        let held_arm = arm_lock.lock().unwrap();
        let n1 = name.clone();
        let t1 = std::thread::spawn(move || VtChannel::acquire(&n1));
        let n2 = name.clone();
        let t2 = std::thread::spawn(move || VtChannel::acquire(&n2));
        let arrival = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&arm_lock) < 4 {
            assert!(
                Instant::now() < arrival,
                "both acquires must reach the held arm lock"
            );
            std::thread::yield_now();
        }
        drop(held_arm);
        drop(arm_lock);
        let r1 = t1.join().expect("thread 1");
        let r2 = t2.join().expect("thread 2");
        assert!(
            r1.is_none_or(|c| c.is_alive()) && r2.is_none_or(|c| c.is_alive()),
            "neither racer may receive a dead channel"
        );
        let other = format!("aoe_test_vt_race_other_{}", std::process::id());
        let _ = VtChannel::acquire(&other);
        assert!(
            !ARM_LOCKS.lock().unwrap().contains_key(&name),
            "arm locks must prune once no acquire is in flight"
        );

        REGISTRY.lock().unwrap().remove(&name);
    }

    #[test]
    fn sample_serves_cache_until_grid_gen_bumps() {
        let name = format!("aoe_test_vt_cache_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel(&name, dir.path());

        ch.parser.lock().unwrap().process(b"one");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let first = ch.sample(4).content;
        assert!(first.contains("one"), "fresh assembly:\n{first:?}");

        ch.parser.lock().unwrap().process(b" two");
        let cached = ch.sample(4).content;
        assert!(
            !cached.contains("two"),
            "same generation must serve the cached assembly:\n{cached:?}"
        );

        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let fresh = ch.sample(4).content;
        assert!(
            fresh.contains("two"),
            "bumped generation must reassemble:\n{fresh:?}"
        );

        let wider = ch.sample(3).content;
        assert!(wider.contains("two"), "window change must reassemble");
    }

    #[test]
    fn seed_replays_application_cursor_mode() {
        let on = PaneSeedState {
            app_cursor: true,
            ..Default::default()
        };
        let mut p = vt100::Parser::new(4, 10, 0);
        p.process(&assemble_seed_stream(b"hi\n", &on, 4));
        assert!(
            p.screen().application_cursor(),
            "keypad_cursor_flag=1 must seed DECCKM"
        );

        let off = PaneSeedState::default();
        let mut p = vt100::Parser::new(4, 10, 0);
        p.process(&assemble_seed_stream(b"hi\n", &off, 4));
        assert!(
            !p.screen().application_cursor(),
            "keypad_cursor_flag=0 must leave DECCKM off"
        );
    }

    #[test]
    fn reader_bumps_grid_gen_per_chunk() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let ctx = ReaderCtx {
            stop: stop.clone(),
            grid_gen: grid_gen.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let mut conn = UnixStream::connect(&sock).expect("connect");
        conn.write_all(b"first-chunk").expect("write");

        let deadline = Instant::now() + Duration::from_secs(5);
        while grid_gen.load(Ordering::Relaxed) < 1 {
            assert!(Instant::now() < deadline, "reader never bumped grid_gen");
            std::thread::sleep(Duration::from_millis(2));
        }

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn idle_reader_leaves_snapshot_available_during_readiness_wait() {
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let snapshot = Arc::new(Mutex::new(()));
        let (waiting, idle_rx, resume_tx) = TestRendezvous::new();
        let ctx = ReaderCtx {
            stop: stop.clone(),
            snapshot: snapshot.clone(),
            ..ReaderCtx::for_test()
        };
        let conn = UnixStream::connect(&sock).expect("connect");
        let reader = std::thread::spawn(move || {
            let mut waiting = Some(waiting);
            run_reader_with_wait(listener, ctx, chunk_now_ms, |_| {
                if let Some(boundary) = waiting.take() {
                    boundary.hold();
                }
                0
            });
        });
        let reached_idle = idle_rx.recv_timeout(Duration::from_secs(5));
        let available = snapshot.try_lock().is_ok();

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        drop(resume_tx);
        let joined = reader.join();

        reached_idle.expect("reader entered the held readiness operation");
        joined.expect("reader exits");
        assert!(
            available,
            "snapshot must stay available during the readiness wait"
        );
    }

    #[test]
    fn reader_keeps_published_input_socket_blocking_under_backpressure() {
        use std::io::{Read, Write};

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let lifecycle = Arc::new(AtomicU8::new(VtLifecycle::Starting as u8));
        let stream = Arc::new(Mutex::new(None));
        let ctx = ReaderCtx {
            stop: stop.clone(),
            stream: stream.clone(),
            lifecycle: lifecycle.clone(),
            ..ReaderCtx::for_test()
        };
        let mut peer = UnixStream::connect(&sock).expect("connect");
        peer.set_read_timeout(Some(Duration::from_secs(5)))
            .expect("read timeout");
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let deadline = Instant::now() + Duration::from_secs(1);
            while VtLifecycle::load(&lifecycle) != VtLifecycle::Live {
                assert!(Instant::now() < deadline, "reader never connected");
                std::thread::sleep(Duration::from_millis(2));
            }

            let mut prefilled = 0;
            {
                let mut published = stream.lock().expect("published stream");
                let input = published.as_mut().expect("reader published input socket");
                let flags = unsafe { libc::fcntl(input.as_raw_fd(), libc::F_GETFL) };
                assert!(flags >= 0, "read input socket flags");
                assert_eq!(flags & libc::O_NONBLOCK, 0, "input socket must block");

                input
                    .set_write_timeout(Some(Duration::from_millis(20)))
                    .unwrap();
                let fill = [b'p'; 4096];
                loop {
                    match input.write(&fill) {
                        Ok(0) => panic!("saturating write made no progress"),
                        Ok(sent) => prefilled += sent,
                        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(err) => {
                            assert!(
                                matches!(
                                    err.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ),
                                "saturating write failed: {err}"
                            );
                            break;
                        }
                    }
                }
                input
                    .set_write_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
            }
            let payload = vec![b'x'; 1024 * 1024];
            let writer_stream = stream.clone();
            let writer_payload = payload.clone();
            let writer = std::thread::spawn(move || {
                writer_stream
                    .lock()
                    .expect("published stream")
                    .as_mut()
                    .expect("reader published input socket")
                    .write_all(&writer_payload)
            });
            let mut prefix = vec![0; prefilled];
            let prefix_read = peer.read_exact(&mut prefix);
            let mut received = vec![0; payload.len()];
            let payload_read = peer.read_exact(&mut received);
            let shutdown = peer.shutdown(std::net::Shutdown::Both);
            let written = writer.join();
            prefix_read.expect("drain saturated socket");
            payload_read.expect("read complete input payload");
            shutdown.expect("shut down native socket");
            written
                .expect("input writer exits")
                .expect("write complete payload");
            assert!(prefix.iter().all(|byte| *byte == b'p'));
            assert_eq!(received, payload, "input payload must arrive exactly once");
        }));

        stop.store(true, Ordering::Relaxed);
        drop(peer);
        let joined = reader.join();
        if let Err(panic) = checked {
            std::panic::resume_unwind(panic);
        }
        joined.expect("reader exits");
    }

    #[test]
    fn seed_swap_abandons_a_chunk_that_landed_during_capture() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            app_cursor: app_cursor.clone(),
            chunk_seq: chunk_seq.clone(),
            settled_chunk_seq: settled_chunk_seq.clone(),
            grid_gen: grid_gen.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        let since = grid_gen.load(Ordering::Relaxed);
        conn.write_all(b"post-snapshot-chunk").expect("write");
        let deadline = Instant::now() + Duration::from_secs(5);
        while grid_gen.load(Ordering::Relaxed) == since {
            assert!(Instant::now() < deadline, "reader never applied the chunk");
            std::thread::sleep(Duration::from_millis(2));
        }

        let seed = assemble_seed_stream(b"snapshot-body\n", &PaneSeedState::default(), 24);
        assert_eq!(
            swap_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                Some(since),
                &seed,
                (80, 24),
                SeedGuard {
                    chunk: Some((&chunk_seq, &settled_chunk_seq, 0)),
                    pipe: None,
                },
            ),
            VtRefreshResult::Busy,
            "swap must stand down once a chunk has landed"
        );
        let grid = parser.lock().expect("parser").screen().contents();
        assert!(
            grid.contains("post-snapshot-chunk"),
            "the raced chunk must survive in the live grid:\n{grid:?}"
        );

        let quiet = grid_gen.load(Ordering::Relaxed);
        let expected_chunk_seq = chunk_seq.load(Ordering::Acquire);
        assert_eq!(
            swap_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                Some(quiet),
                &seed,
                (80, 24),
                SeedGuard {
                    chunk: Some((&chunk_seq, &settled_chunk_seq, expected_chunk_seq)),
                    pipe: None,
                },
            ),
            VtRefreshResult::Refreshed,
            "an unraced swap must apply the snapshot"
        );
        let grid = parser.lock().expect("parser").screen().contents();
        assert!(
            grid.contains("snapshot-body") && !grid.contains("post-snapshot-chunk"),
            "snapshot must replace the grid:\n{grid:?}"
        );

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn seed_counts_output_from_before_the_capture_fork_as_captured() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            app_cursor: app_cursor.clone(),
            chunk_seq: chunk_seq.clone(),
            settled_chunk_seq: settled_chunk_seq.clone(),
            grid_gen: grid_gen.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let mut conn = UnixStream::connect(&sock).expect("connect");
        let links = LinkTable::default();
        let wait_settled = |n: u64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while settled_chunk_seq.load(Ordering::Acquire) < n {
                assert!(Instant::now() < deadline, "reader never settled chunk {n}");
                std::thread::sleep(Duration::from_millis(2));
            }
        };
        let no_fence = || SeedInstallFence {
            snapshot: None,
            socket: None,
            control: None,
        };
        let sink = || SeedSink {
            parser: &parser,
            app_cursor: &app_cursor,
            grid_gen: &grid_gen,
            links: &links,
        };

        let result = seed_parser_with(
            sink(),
            true,
            (80, 24),
            Some((&chunk_seq, &settled_chunk_seq)),
            no_fence(),
            |sample| {
                conn.write_all(b"probe-window-chunk").expect("write");
                wait_settled(1);
                let sampled = sample();
                let body = assemble_seed_stream(b"snapshot-body\n", &PaneSeedState::default(), 24);
                Some((body, sampled))
            },
        );
        assert_eq!(
            result,
            VtRefreshResult::Refreshed,
            "output settled before the capture fork is part of the snapshot"
        );
        let grid = parser.lock().expect("parser").screen().contents();
        assert!(
            grid.contains("snapshot-body"),
            "snapshot must be installed:\n{grid:?}"
        );

        let result = seed_parser_with(
            sink(),
            true,
            (80, 24),
            Some((&chunk_seq, &settled_chunk_seq)),
            no_fence(),
            |sample| {
                let sampled = sample();
                conn.write_all(b"capture-window-chunk").expect("write");
                wait_settled(2);
                let body = assemble_seed_stream(b"stale-snapshot\n", &PaneSeedState::default(), 24);
                Some((body, sampled))
            },
        );
        assert_eq!(
            result,
            VtRefreshResult::Busy,
            "output after the capture fork must still fence the install"
        );
        let grid = parser.lock().expect("parser").screen().contents();
        assert!(
            grid.contains("capture-window-chunk") && !grid.contains("stale-snapshot"),
            "the raced chunk must survive in the live grid:\n{grid:?}"
        );

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn unread_pipe_chunk_blocks_snapshot_replay() {
        use std::io::{Read, Write};

        let (mut reader, mut writer) = UnixStream::pair().expect("pipe pair");
        writer.write_all(b"UNREAD-MARKER").expect("queue output");

        let parser = Mutex::new(vt100::Parser::new(24, 80, 0));
        let app_cursor = AtomicBool::new(false);
        let grid_gen = AtomicU64::new(0);
        let chunk_seq = AtomicU64::new(0);
        let settled_chunk_seq = AtomicU64::new(0);
        let seed_state = PaneSeedState {
            cursor_x: b"UNREAD-MARKER".len() as u16,
            ..PaneSeedState::default()
        };
        let seed = assemble_seed_stream(b"UNREAD-MARKER\n", &seed_state, 24);

        assert_eq!(
            swap_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                &seed,
                (80, 24),
                SeedGuard {
                    chunk: Some((&chunk_seq, &settled_chunk_seq, 0)),
                    pipe: Some(&reader),
                },
            ),
            VtRefreshResult::Busy,
            "a snapshot must not overtake output still queued in the pipe",
        );

        let mut unread = [0; b"UNREAD-MARKER".len()];
        reader.read_exact(&mut unread).expect("drain output");
        parser.lock().unwrap().process(&unread);

        assert_eq!(
            swap_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                &seed,
                (80, 24),
                SeedGuard {
                    chunk: Some((&chunk_seq, &settled_chunk_seq, 0)),
                    pipe: Some(&reader),
                },
            ),
            VtRefreshResult::Refreshed,
            "a drained pipe allows the snapshot to install",
        );

        let contents = parser.lock().unwrap().screen().contents();
        assert!(
            contents.matches("UNREAD-MARKER").count() == 1,
            "the unread pipe chunk must be applied exactly once:\n{contents:?}"
        );
    }

    #[test]
    fn forwarder_read_barrier_blocks_snapshot_installation() {
        use std::io::{Read, Write};
        use std::sync::mpsc;

        let (pane_reader, mut pane_writer) = UnixStream::pair().expect("pane pair");
        let (mut reader, forwarder) = UnixStream::pair().expect("data pair");
        let (parent_control, forwarder_control) = UnixStream::pair().expect("control pair");
        let (read_tx, read_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let forwarder_thread = std::thread::spawn(move || {
            let mut pause_once = true;
            pump_pane_output_with_hook(
                pane_reader.as_raw_fd(),
                &forwarder,
                Some(&forwarder_control),
                &mut || {
                    if pause_once {
                        pause_once = false;
                        read_tx.send(()).expect("signal forwarder read");
                        resume_rx.recv().expect("resume forwarder");
                    }
                },
            );
        });

        pane_writer
            .write_all(b"FORWARDER-MARKER")
            .expect("queue pane output");
        read_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("forwarder must pause after reading pane output");

        let parser = Mutex::new(vt100::Parser::new(6, 40, 0));
        let app_cursor = AtomicBool::new(false);
        let grid_gen = AtomicU64::new(0);
        let control = Mutex::new(DrainControl {
            stream: Some(parent_control),
            next_generation: 0,
            before_deadline: None,
            next_now: None,
        });
        assert_eq!(
            swap_drained_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                b"FORWARDER-MARKER\r\n",
                (40, 6),
                DrainedSeedGuard {
                    guard: SeedGuard {
                        chunk: None,
                        pipe: Some(&reader),
                    },
                    control: &control,
                },
            ),
            VtRefreshResult::Busy,
            "a snapshot must stand down while the forwarder owns a captured byte"
        );

        resume_tx.send(()).expect("resume forwarder");
        let mut forwarded = [0; b"FORWARDER-MARKER".len()];
        reader.read_exact(&mut forwarded).expect("forwarded marker");
        assert_eq!(&forwarded, b"FORWARDER-MARKER");
        drop(pane_writer);
        forwarder_thread.join().expect("forwarder exits");
    }

    #[test]
    fn entered_drain_leaves_input_socket_available() {
        use std::io::{Read, Write};

        let (mut data_reader, data_forwarder) = UnixStream::pair().expect("data pair");
        let (parent_control, mut forwarder_control) = UnixStream::pair().expect("control pair");
        forwarder_control
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("bound probe wait");
        let (before_deadline, entered_rx, resume_tx) = TestRendezvous::new();
        let socket = Arc::new(Mutex::new(Some(data_forwarder)));
        let control = Mutex::new(DrainControl {
            stream: Some(parent_control),
            next_generation: 0,
            before_deadline: Some(before_deadline),
            next_now: None,
        });
        let parser = Mutex::new(vt100::Parser::new(6, 40, 0));
        let app_cursor = AtomicBool::new(false);
        let grid_gen = AtomicU64::new(0);
        let snapshot = Mutex::new(());
        let seed_socket = Arc::clone(&socket);
        let seed = std::thread::spawn(move || {
            install_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                b"INPUT-MUTEX-SEED\r\n",
                (40, 6),
                SeedGuard {
                    chunk: None,
                    pipe: None,
                },
                SeedInstallFence {
                    snapshot: Some(&snapshot),
                    socket: Some(&seed_socket),
                    control: Some(&control),
                },
            )
        });

        let entered = entered_rx.recv_timeout(Duration::from_secs(5));
        let wrote_input = socket
            .try_lock()
            .map(|mut guard| {
                guard
                    .as_mut()
                    .is_some_and(|stream| stream.write_all(b"x").is_ok())
            })
            .unwrap_or(false);
        let mut input = [0; 1];
        let received_input = wrote_input && data_reader.read_exact(&mut input).is_ok();

        let resumed = resume_tx.send(());
        drop(resume_tx);
        let probe = read_drain_frame(&mut forwarder_control);
        if let Ok((DRAIN_PROBE, generation)) = probe {
            let _ = forwarder_control.write_all(&drain_frame(DRAIN_ACK, generation));
        }
        let result = seed.join();

        entered.expect("drain is held after acquiring control and before its deadline");
        resumed.expect("resume drain");
        assert!(
            received_input,
            "input must traverse the socket while drain is held"
        );
        assert_eq!(&input, b"x");
        assert!(matches!(probe, Ok((DRAIN_PROBE, _))), "receive drain probe");
        assert!(matches!(
            result.expect("seed exits"),
            VtRefreshResult::Busy | VtRefreshResult::Refreshed
        ));
    }

    #[test]
    fn drain_timeout_ignores_late_ack_and_recovers_on_retry() {
        use std::io::{Read, Write};
        use std::sync::mpsc;

        let (mut data_reader, mut data_forwarder) = UnixStream::pair().expect("data pair");
        let (parent_control, mut forwarder_control) = UnixStream::pair().expect("control pair");
        let (probe_tx, probe_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let (late_ack_tx, late_ack_rx) = mpsc::channel();
        let (matching_tx, matching_rx) = mpsc::channel();
        let (written_tx, written_rx) = mpsc::channel();
        forwarder_control
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let forwarder = std::thread::spawn(move || {
            let (kind, generation) =
                read_drain_frame(&mut forwarder_control).expect("receive first drain probe");
            assert_eq!(kind, DRAIN_PROBE);
            probe_tx.send(()).expect("signal held probe");
            resume_rx.recv().expect("release held pane byte");
            data_forwarder
                .write_all(b"RETRY-MARKER")
                .expect("forward held pane byte");
            late_ack_tx
                .send(
                    forwarder_control
                        .write_all(&drain_frame(DRAIN_ACK, generation))
                        .is_ok(),
                )
                .expect("report late ack");
            let (kind, generation) =
                read_drain_frame(&mut forwarder_control).expect("receive retry drain probe");
            assert_eq!(kind, DRAIN_PROBE);
            matching_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("release matching ACK");
            forwarder_control
                .write_all(&drain_frame(DRAIN_ACK, generation))
                .expect("acknowledge retry probe");
            forwarder_control
                .write_all(&drain_frame(DRAIN_ACK, generation.wrapping_add(1)))
                .expect("prequeue seed ACK");
            written_tx.send(()).expect("matching and seed ACKs written");
            let (kind, next) =
                read_drain_frame(&mut forwarder_control).expect("receive seed probe");
            assert_eq!((kind, next), (DRAIN_PROBE, generation.wrapping_add(1)));
        });

        let control = Mutex::new(DrainControl {
            stream: Some(parent_control),
            next_generation: 0,
            before_deadline: None,
            next_now: None,
        });
        let parser = Mutex::new(vt100::Parser::new(6, 40, 0));
        let app_cursor = AtomicBool::new(false);
        let grid_gen = AtomicU64::new(0);
        let first = swap_drained_seeded_parser(
            SeedSink {
                parser: &parser,
                app_cursor: &app_cursor,
                grid_gen: &grid_gen,
                links: &LinkTable::default(),
            },
            None,
            b"RETRY-MARKER\r\n",
            (40, 6),
            DrainedSeedGuard {
                guard: SeedGuard {
                    chunk: None,
                    pipe: Some(&data_reader),
                },
                control: &control,
            },
        );
        probe_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("forwarder receives first probe");
        assert_eq!(
            first,
            VtRefreshResult::Busy,
            "timed-out drain must stand down"
        );
        assert!(
            control.lock().unwrap().stream.is_some(),
            "a timed-out control connection remains available for a correlated retry"
        );

        resume_tx.send(()).expect("release forwarder");
        let mut marker = [0; b"RETRY-MARKER".len()];
        data_reader
            .read_exact(&mut marker)
            .expect("drain late pane byte");
        assert_eq!(&marker, b"RETRY-MARKER");
        assert!(
            late_ack_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("late ack result"),
            "the control stream remains open so the retry can observe generations"
        );

        let (second_read, rejected_rx, resume_read) = TestRendezvous::new();
        let (rejected, written, retried) = std::thread::scope(|scope| {
            let control = &control;
            let retry = scope.spawn(move || {
                let now = Instant::now();
                let mut reads = 0;
                let mut second_read = Some(second_read);
                drain_forwarder_with_io(
                    control,
                    || now,
                    |stream| {
                        reads += 1;
                        if reads == 2 && !second_read.take().unwrap().hold() {
                            return Err(std::io::ErrorKind::Interrupted.into());
                        }
                        read_drain_frame(stream)
                    },
                )
            });
            let rejected = rejected_rx.recv_timeout(Duration::from_secs(5));
            let _ = matching_tx.send(());
            let written = written_rx.recv_timeout(Duration::from_secs(5));
            let _ = resume_read.send(());
            (rejected, written, retry.join())
        });

        control.lock().unwrap().next_now = Some(Instant::now());
        assert_eq!(
            swap_drained_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                b"RETRY-MARKER\r\n",
                (40, 6),
                DrainedSeedGuard {
                    guard: SeedGuard {
                        chunk: None,
                        pipe: Some(&data_reader),
                    },
                    control: &control,
                },
            ),
            VtRefreshResult::Refreshed,
            "the correlated connection remains usable for the seed retry"
        );
        forwarder.join().expect("forwarder exits");
        rejected.expect("drain rejected the stale ACK before requesting another frame");
        written.expect("matching ACK was written before the next read");
        assert!(
            retried.expect("retry exits"),
            "matching ACK completes the retry"
        );
    }

    #[test]
    fn grid_content_preserves_color() {
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

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let lifecycle = Arc::new(AtomicU8::new(VtLifecycle::Starting as u8));
        let wakeup_slot: Arc<Mutex<Option<ChangeWakeup>>> = Arc::new(Mutex::new(None));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            stream,
            lifecycle: lifecycle.clone(),
            wakeup: wakeup_slot.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        let pair: ChangeWakeup = Arc::new((Mutex::new(0), Condvar::new()));
        *wakeup_slot.lock().unwrap() = Some(pair.clone());
        let guard = pair.0.lock().unwrap();
        conn.write_all(b"echo-marker").expect("write pane output");
        let (wake_guard, res) = pair
            .1
            .wait_timeout_while(guard, Duration::from_secs(5), |generation| *generation == 0)
            .expect("wait");
        drop(wake_guard);
        assert!(
            !res.timed_out(),
            "a grid change must poke the registered wakeup"
        );
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
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;aGk\x07"), Some("hi".to_string()));
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;;aGVsbG8=\x07"), Some("hello".to_string()));
    }

    #[test]
    fn osc52_scanner_survives_arbitrary_chunk_splits() {
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
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;?\x07"), None);
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;\x07"), None);
        let mut s = Osc52Scanner::new();
        assert_eq!(s.feed(b"\x1b]52;c;=====\x07"), None);
    }

    #[test]
    fn osc52_scanner_ignores_other_sequences_and_recovers() {
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1b]0;title\x07\x1b[31m\x1b]521;x\x07\x1b]52;c;aGVsbG8=\x07"),
            Some("hello".to_string())
        );
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1b]52;c;aGVsbG8=\x07\x1b]52;c;aGk=\x07"),
            Some("hi".to_string())
        );
    }

    #[test]
    fn osc52_scanner_unwraps_tmux_passthrough_wrapped_writes() {
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x07\x1b\\"),
            Some("hello".to_string())
        );
        let mut s = Osc52Scanner::new();
        assert_eq!(
            s.feed(b"\x1bPtmux;\x1b\x1b]52;c;aGVsbG8=\x1b\x1b\\\x1b\\"),
            Some("hello".to_string())
        );
    }

    #[test]
    fn reader_publishes_osc52_clipboard_from_pane_stream() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let lifecycle = Arc::new(AtomicU8::new(VtLifecycle::Starting as u8));
        let clipboard: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            lifecycle: lifecycle.clone(),
            clipboard: clipboard.clone(),
            ..ReaderCtx::for_test()
        };
        let settled = ctx.settled_chunk_seq.clone();
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
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
        while settled.load(Ordering::Acquire) < 1 {
            assert!(
                Instant::now() < deadline,
                "reader never settled copied chunk"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
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
    fn reader_records_osc8_targets_the_grid_drops() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let links: Arc<LinkTable> = Arc::new(LinkTable::default());
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            links: links.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let mut conn = UnixStream::connect(&sock).expect("connect");
        conn.write_all(b"see \x1b]8;;https://example.com/repo\x1b\\the repo\x1b]8;;\x1b\\ now")
            .expect("write pane output");

        let deadline = Instant::now() + Duration::from_secs(5);
        while !parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("see the repo now")
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        drop(conn);
        reader.join().expect("reader thread");
        let recorded: Vec<PaneLink> = links.table.lock().unwrap().iter().cloned().collect();
        assert_eq!(
            recorded,
            vec![PaneLink {
                text: "the repo".to_string(),
                uri: "https://example.com/repo".to_string(),
            }]
        );
        assert!(
            parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("see the repo now"),
            "the grid keeps the visible text and none of the sequence"
        );
    }

    fn tmux_reemits_hyperlinks() -> bool {
        let Ok(out) = crate::tmux::tmux_command().arg("-V").output() else {
            return false;
        };
        if !out.status.success() {
            return false;
        }
        const TMUX_OSC8_MIN: (u32, u32) = (3, 4);
        tmux_version(&String::from_utf8_lossy(out.stdout.as_slice())) >= TMUX_OSC8_MIN
    }

    fn tmux_version(version: &str) -> (u32, u32) {
        let digits: String = version
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut parts = digits.split('.');
        (
            parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
            parts.next().and_then(|p| p.parse().ok()).unwrap_or(0),
        )
    }

    fn record_seed_links(slot: &LinkTable, stream: &[u8]) {
        reconcile_links(slot, crate::tmux::osc8::extract_links(stream));
    }
    #[test]
    #[serial_test::serial]
    fn real_tmux_seed_lands_the_cursor_on_the_prompt_at_a_shorter_grid() {
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("Skipping test: tmux unavailable");
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_seed_geom");
        let script = "for i in $(seq 1 20); do echo \"line-$i\"; done; printf 'READY> '; sleep 30";
        let out = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                guard.name(),
                "-x",
                "80",
                "-y",
                "40",
                script,
            ])
            .output()
            .expect("tmux new-session");
        assert!(out.status.success());
        let target = crate::tmux::test_helpers::only_pane_id(guard.name());
        let deadline = crate::tmux::TmuxCommandDeadline::new();

        let mut probe = PaneSeedState::default();
        for _ in 0..50 {
            probe = pane_seed_state(&target, &deadline).unwrap_or_default();
            if probe.cursor_y == 20 && probe.cursor_x == 7 {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            (probe.pane_height, probe.cursor_y, probe.cursor_x),
            (40, 20, 7),
            "fixture must park the cursor on the prompt row of a 40-row pane"
        );

        let rows: u16 = 24;
        let stream = capture_seed_stream(&target, (80, rows), &deadline, || ())
            .expect("capture seed stream")
            .0;
        let mut p = vt100::Parser::new(rows, 80, SCROLLBACK_LINES);
        p.process(&stream);

        let (cy, cx) = p.screen().cursor_position();
        let contents = p.screen().contents();
        let prompt_row = contents
            .lines()
            .position(|l| l.contains("READY>"))
            .expect("prompt must be on the visible screen");
        assert_eq!(
            (cy as usize, cx),
            (prompt_row, 7),
            "cursor must sit on the prompt row the shorter grid pushed up:\n{contents}"
        );
        assert_eq!(
            contents.lines().filter(|l| l.contains("READY>")).count(),
            1,
            "one prompt row only:\n{contents}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn real_tmux_capture_carries_hyperlinks_into_the_link_table() {
        if !tmux_reemits_hyperlinks() {
            eprintln!("Skipping test: tmux missing or older than 3.4 (no OSC 8)");
            return;
        }
        let guard = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_osc8_seed");
        let script = concat!(
            r"printf 'A: \033]8;;https://example.com/mid\033\\mid link\033]8;;\033\\ after\n'; ",
            r"printf 'B: \033]8;;https://example.com/eol\033\\eol link\033]8;;\033\\\n'; ",
            "sleep 30",
        );
        let out = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                guard.name(),
                "-x",
                "80",
                "-y",
                "24",
                script,
            ])
            .output()
            .expect("tmux new-session");
        assert!(out.status.success());
        let expected = vec![
            PaneLink {
                text: "mid link".to_string(),
                uri: "https://example.com/mid".to_string(),
            },
            PaneLink {
                text: "eol link".to_string(),
                uri: "https://example.com/eol".to_string(),
            },
        ];
        let target = crate::tmux::test_helpers::only_pane_id(guard.name());
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        let mut stream = Vec::new();
        for _ in 0..50 {
            stream = capture_seed_stream(&target, (80, 24), &deadline, || ())
                .map(|(stream, ())| stream)
                .unwrap_or_default();
            if crate::tmux::osc8::extract_links(&stream) == expected {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        let slot = LinkTable::default();
        record_seed_links(&slot, &stream);
        let held: Vec<PaneLink> = slot.table.lock().unwrap().iter().cloned().collect();
        assert_eq!(
            held, expected,
            "capture-pane -e must round-trip both hyperlink shapes"
        );
    }

    #[test]
    fn seed_records_links_already_on_screen() {
        let slot = LinkTable::default();
        record_seed_links(
            &slot,
            b"\x1b[32msee \x1b]8;;https://example.com/repo\x1b\\the repo\x1b]8;;\x1b\\ now\x1b[0m",
        );
        assert_eq!(
            slot.table
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![PaneLink {
                text: "the repo".to_string(),
                uri: "https://example.com/repo".to_string(),
            }]
        );
        record_seed_links(
            &slot,
            b"\x1b]8;;https://example.com/repo\x1b\\the repo\x1b]8;;\x1b\\",
        );
        assert_eq!(slot.table.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_accepted_snapshot_replaces_rather_than_merges_links() {
        let slot = LinkTable::default();
        record_links(
            &slot,
            vec![PaneLink {
                text: "docs".to_string(),
                uri: "https://example.com/old".to_string(),
            }],
        );
        let after_record = slot.generation.load(Ordering::Acquire);

        record_seed_links(
            &slot,
            b"see \x1b]8;;https://example.com/new\x1b\\docs\x1b]8;;\x1b\\ now",
        );
        let held: Vec<PaneLink> = slot.table.lock().unwrap().iter().cloned().collect();
        assert_eq!(held.len(), 1, "the obsolete target is gone: {held:?}");
        assert_eq!(held[0].uri, "https://example.com/new");
        assert!(slot.generation.load(Ordering::Acquire) > after_record);

        record_seed_links(&slot, b"see docs now");
        assert!(
            slot.table.lock().unwrap().is_empty(),
            "a snapshot with no sequences must leave no targets"
        );
    }

    #[test]
    fn a_rejected_swap_leaves_the_links_alone() {
        let slot = LinkTable::default();
        record_links(
            &slot,
            vec![PaneLink {
                text: "docs".to_string(),
                uri: "https://example.com/live".to_string(),
            }],
        );
        let parser = Mutex::new(vt100::Parser::new(24, 80, 0));
        let app_cursor = AtomicBool::new(false);
        let grid_gen = AtomicU64::new(7);
        assert_eq!(
            swap_seeded_parser(
                SeedSink {
                    parser: &parser,
                    app_cursor: &app_cursor,
                    grid_gen: &grid_gen,
                    links: &slot,
                },
                Some(1),
                b"\x1b]8;;https://example.com/stale\x1b\\docs\x1b]8;;\x1b\\",
                (80, 24),
                SeedGuard {
                    chunk: None,
                    pipe: None,
                },
            ),
            VtRefreshResult::Busy
        );
        let held: Vec<PaneLink> = slot.table.lock().unwrap().iter().cloned().collect();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].uri, "https://example.com/live");
    }

    #[test]
    fn an_install_holds_the_snapshot_fence_across_its_swap() {
        use std::io::Write;

        let (_data_reader, data_forwarder) = UnixStream::pair().expect("data pair");
        let (parent_control, mut forwarder_control) = UnixStream::pair().expect("control pair");
        let snapshot = Arc::new(Mutex::new(()));
        let (before_deadline, entered, resume) = TestRendezvous::new();

        let socket = Arc::new(Mutex::new(Some(data_forwarder)));
        let control = Mutex::new(DrainControl {
            stream: Some(parent_control),
            next_generation: 0,
            before_deadline: Some(before_deadline),
            next_now: Some(Instant::now()),
        });
        let parser = Mutex::new(vt100::Parser::new(6, 40, 0));
        let app_cursor = AtomicBool::new(false);
        let grid_gen = AtomicU64::new(0);
        let links = LinkTable::default();

        let (result, entered, ack, resumed, reached_swap, fenced_at_drain, fenced_in_swap) =
            std::thread::scope(|scope| {
                let resume = resume;
                let in_swap = links.table.lock().expect("hold the link table");
                let install = scope.spawn(|| {
                    install_seeded_parser(
                        SeedSink {
                            parser: &parser,
                            app_cursor: &app_cursor,
                            grid_gen: &grid_gen,
                            links: &links,
                        },
                        None,
                        b"\x1b]8;;https://example.com/seeded\x1b\\docs\x1b]8;;\x1b\\\r\n",
                        (40, 6),
                        SeedGuard {
                            chunk: None,
                            pipe: None,
                        },
                        SeedInstallFence {
                            snapshot: Some(&snapshot),
                            socket: Some(&socket),
                            control: Some(&control),
                        },
                    )
                });
                let entered = entered.recv_timeout(Duration::from_secs(5));
                let fenced_at_drain = snapshot.try_lock().is_err();
                let ack = forwarder_control.write_all(&drain_frame(DRAIN_ACK, 0));
                let resumed = resume.send(());
                drop(resume);
                let arrival = Instant::now() + Duration::from_secs(5);
                while parser.try_lock().is_ok()
                    && Instant::now() < arrival
                    && !install.is_finished()
                {
                    std::thread::yield_now();
                }
                let reached_swap = parser.try_lock().is_err();
                let fenced = snapshot.try_lock().is_err();
                drop(in_swap);
                (
                    install.join(),
                    entered,
                    ack,
                    resumed,
                    reached_swap,
                    fenced_at_drain,
                    fenced,
                )
            });

        entered.expect("install reached native drain");
        ack.expect("prequeue native ACK");
        resumed.expect("release drain");
        assert!(reached_swap, "the install never reached the swap");
        let result = result.expect("install thread");

        assert_eq!(result, VtRefreshResult::Refreshed);
        assert!(
            fenced_at_drain,
            "the install must hold the fence across its drain"
        );
        assert!(
            fenced_in_swap,
            "and still hold it inside the swap, where the link table is replaced"
        );
        assert_eq!(
            links
                .table
                .lock()
                .unwrap()
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec![PaneLink {
                text: "docs".to_string(),
                uri: "https://example.com/seeded".to_string(),
            }]
        );
    }

    #[test]
    fn a_reseed_cannot_erase_a_link_recorded_inside_its_fence() {
        use std::io::Write;
        use std::sync::mpsc;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let links: Arc<LinkTable> = Arc::new(LinkTable::default());
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let snapshot = Arc::new(Mutex::new(()));
        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (contended_tx, contended_rx) = mpsc::channel();
        let ctx = ReaderCtx {
            snapshot_contended: Some(contended_tx),
            parser: parser.clone(),
            stop: stop.clone(),
            snapshot: snapshot.clone(),
            stream: stream.clone(),
            app_cursor: app_cursor.clone(),
            links: links.clone(),
            chunk_seq: chunk_seq.clone(),
            settled_chunk_seq: settled_chunk_seq.clone(),
            grid_gen: grid_gen.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        conn.write_all(b"see docs now").expect("write pane output");
        let ready = Instant::now() + Duration::from_secs(5);
        while !parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("see docs now")
        {
            assert!(
                Instant::now() < ready,
                "reader never applied the first chunk"
            );
            std::thread::sleep(Duration::from_millis(2));
        }

        let (parent_control, mut forwarder_control) = UnixStream::pair().expect("control pair");
        let (probed_tx, probed_rx) = mpsc::channel();
        let forwarder = std::thread::spawn(move || {
            let (kind, generation) =
                read_drain_frame(&mut forwarder_control).expect("receive drain probe");
            assert_eq!(kind, DRAIN_PROBE);
            probed_tx.send(()).expect("signal the probe arrived");
            let _ = forwarder_control.write_all(&drain_frame(DRAIN_ACK, generation));
        });
        let control = Arc::new(Mutex::new(DrainControl {
            stream: Some(parent_control),
            next_generation: 0,
            before_deadline: None,
            next_now: None,
        }));

        let expected_chunk_seq = chunk_seq.load(Ordering::Acquire);
        let seed = assemble_seed_stream(b"see docs now\n", &PaneSeedState::default(), 24);

        let fence = snapshot.lock().expect("hold the fence");
        conn.write_all(b"\r\n\x1b]8;;https://example.com/new\x1b\\docs\x1b]8;;\x1b\\ added")
            .expect("write pane output");
        let install = {
            let (parser, app_cursor, grid_gen, links) = (
                parser.clone(),
                app_cursor.clone(),
                grid_gen.clone(),
                links.clone(),
            );
            let (chunk_seq, settled_chunk_seq) = (chunk_seq.clone(), settled_chunk_seq.clone());
            let (snapshot, stream, control) = (snapshot.clone(), stream.clone(), control.clone());
            std::thread::spawn(move || {
                install_seeded_parser(
                    SeedSink {
                        parser: &parser,
                        app_cursor: &app_cursor,
                        grid_gen: &grid_gen,
                        links: &links,
                    },
                    None,
                    &seed,
                    (80, 24),
                    SeedGuard {
                        chunk: Some((&chunk_seq, &settled_chunk_seq, expected_chunk_seq)),
                        pipe: None,
                    },
                    SeedInstallFence {
                        snapshot: Some(&snapshot),
                        socket: Some(&stream),
                        control: Some(&control),
                    },
                )
            })
        };

        let contention = contended_rx.recv_timeout(Duration::from_secs(5));
        let fenced_seq = chunk_seq.load(Ordering::Acquire);
        let fenced_links_empty = links.table.lock().unwrap().is_empty();

        drop(fence);
        probed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the install proceeds once the fence clears");
        assert_eq!(
            install.join().expect("install thread"),
            VtRefreshResult::Busy,
            "whichever side wins the released fence, the snapshot is stale: the chunk is either unread on the socket or already past the baseline it captured at"
        );

        let landed = Instant::now() + Duration::from_secs(5);
        while !parser
            .lock()
            .unwrap()
            .screen()
            .contents()
            .contains("docs added")
            && Instant::now() < landed
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        drop(conn);
        reader.join().expect("reader thread");
        forwarder.join().expect("forwarder thread");
        contention.expect("reader attempted the held snapshot fence");
        assert_eq!(
            fenced_seq, expected_chunk_seq,
            "the held fence excludes recv"
        );
        assert!(fenced_links_empty);
        let recorded: Vec<PaneLink> = links.table.lock().unwrap().iter().cloned().collect();
        assert_eq!(
            recorded,
            vec![PaneLink {
                text: "docs".to_string(),
                uri: "https://example.com/new".to_string(),
            }],
            "the newly advertised target must survive the reseed"
        );
        assert!(
            parser
                .lock()
                .unwrap()
                .screen()
                .contents()
                .contains("docs added"),
            "and the label it describes must be on the grid"
        );
    }

    #[test]
    fn record_links_dedupes_and_caps() {
        let slot = LinkTable::default();
        let link = |n: usize| PaneLink {
            text: format!("link {n}"),
            uri: format!("https://example.com/{n}"),
        };
        record_links(&slot, vec![link(0), link(1), link(0)]);
        assert_eq!(
            slot.table
                .lock()
                .unwrap()
                .iter()
                .map(|l| l.uri.clone())
                .collect::<Vec<_>>(),
            vec!["https://example.com/1", "https://example.com/0"]
        );
        record_links(
            &slot,
            (2..crate::tmux::osc8::MAX_PANE_LINKS + 8)
                .map(link)
                .collect(),
        );
        let held = slot.table.lock().unwrap();
        assert_eq!(held.len(), crate::tmux::osc8::MAX_PANE_LINKS);
        assert_eq!(
            held.back().unwrap().uri,
            format!(
                "https://example.com/{}",
                crate::tmux::osc8::MAX_PANE_LINKS + 7
            )
        );
    }

    #[test]
    fn osc52_observer_publishes_copy_without_a_vt_grid() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(false));
        let clipboard = Arc::new(Mutex::new(None));
        let clipboard_seq = Arc::new(AtomicU64::new(0));
        let reader = {
            let stop = stop.clone();
            let alive = alive.clone();
            let clipboard = clipboard.clone();
            let clipboard_seq = clipboard_seq.clone();
            std::thread::spawn(move || {
                run_osc52_reader(listener, stop, alive, clipboard, clipboard_seq)
            })
        };
        let mut conn = UnixStream::connect(&sock).expect("connect");
        conn.write_all(b"\x1b]52;c;aGVsbG8=\x07")
            .expect("write pane output");

        let deadline = Instant::now() + Duration::from_secs(5);
        while clipboard_seq.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "observer never received OSC 52");
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut existing_viewer = 0;
        let mut newly_connected_viewer = clipboard_seq.load(Ordering::Acquire);
        assert_eq!(
            osc52_clipboard_after(&clipboard, &clipboard_seq, &mut existing_viewer).as_deref(),
            Some("hello")
        );
        assert_eq!(
            osc52_clipboard_after(&clipboard, &clipboard_seq, &mut newly_connected_viewer),
            None,
            "a new viewer must baseline rather than replay an old copy"
        );
        conn.write_all(b"\x1b]52;c;d29ybGQ=\x07")
            .expect("write second pane output");
        while clipboard_seq.load(Ordering::Acquire) < 2 {
            assert!(
                Instant::now() < deadline,
                "observer never received second OSC 52"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            osc52_clipboard_after(&clipboard, &clipboard_seq, &mut existing_viewer).as_deref(),
            Some("world")
        );
        assert_eq!(
            osc52_clipboard_after(&clipboard, &clipboard_seq, &mut newly_connected_viewer)
                .as_deref(),
            Some("world"),
            "each viewer must observe the new copy independently"
        );
        assert!(alive.load(Ordering::Relaxed), "observer never became live");

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn reader_chunk_timing_distinguishes_stream_from_lone_chunk() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let parser = Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0)));
        let stop = Arc::new(AtomicBool::new(false));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let last_chunk_ms = Arc::new(AtomicU64::new(0));
        let prev_gap_ms = Arc::new(AtomicU64::new(u64::MAX));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            chunk_seq,
            settled_chunk_seq: settled_chunk_seq.clone(),
            last_chunk_ms: last_chunk_ms.clone(),
            prev_gap_ms: prev_gap_ms.clone(),
            ..ReaderCtx::for_test()
        };
        let now = Arc::new(AtomicU64::new(100));
        let reader_now = now.clone();
        let reader = std::thread::spawn(move || {
            run_reader(listener, ctx, || reader_now.load(Ordering::Acquire))
        });
        let mut conn = UnixStream::connect(&sock).expect("connect");

        let wait_seq = |n: u64| {
            let deadline = Instant::now() + Duration::from_secs(5);
            while settled_chunk_seq.load(Ordering::Acquire) < n {
                assert!(
                    Instant::now() < deadline,
                    "reader did not settle {n} chunks"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        };

        for (index, (at, bytes, gap)) in [
            (100, &b"\x1b[2J"[..], u64::MAX),
            (104, &b"partial"[..], 4),
            (109, &b" repaint"[..], 5),
            (149, &b"!"[..], 40),
        ]
        .into_iter()
        .enumerate()
        {
            now.store(at, Ordering::Release);
            conn.write_all(bytes).expect("write chunk");
            wait_seq(index as u64 + 1);
            assert_eq!(last_chunk_ms.load(Ordering::Relaxed), at);
            assert_eq!(prev_gap_ms.load(Ordering::Relaxed), gap);
        }
        assert_eq!(
            parser.lock().unwrap().screen().contents(),
            "partial repaint!"
        );

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn grid_content_assembles_scrollback_and_screen() {
        let mut p = vt100::Parser::new(4, 20, 100);
        for i in 1..=12 {
            p.process(format!("LINE{i:02}\r\n").as_bytes());
        }

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

        let (screen_only, _) = grid_content(&mut p, 4, 20, 4);
        assert!(
            !screen_only.contains("LINE01"),
            "screen-only window should not include scrollback:\n{screen_only}"
        );
        assert_eq!(p.screen().scrollback(), 0, "live-edge offset not restored");
    }

    #[test]
    fn reader_fences_seed_windows_and_still_taps_clipboard() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let seeded = Arc::new(AtomicBool::new(false));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(6, 40, 0)));
        let clipboard: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let snapshot = Arc::new(Mutex::new(()));
        let stream = Arc::new(Mutex::new(None));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            seeded: seeded.clone(),
            snapshot: snapshot.clone(),
            stream: stream.clone(),
            app_cursor: app_cursor.clone(),
            clipboard: clipboard.clone(),
            chunk_seq: chunk_seq.clone(),
            settled_chunk_seq: settled_chunk_seq.clone(),
            grid_gen: grid_gen.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, chunk_now_ms));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        conn.write_all(b"PRE-SEED-OUTPUT\x1b]52;c;aGVsbG8=\x07")
            .expect("write pre-seed");
        let deadline = Instant::now() + Duration::from_secs(5);
        while settled_chunk_seq.load(Ordering::Acquire) < 1 {
            assert!(
                Instant::now() < deadline,
                "reader never settled the pre-seed chunk"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            clipboard.lock().unwrap().as_deref(),
            Some("hello"),
            "clipboard must still be tapped while arming"
        );
        assert_eq!(
            grid_gen.load(Ordering::Relaxed),
            0,
            "a dropped pre-seed chunk must not bump the grid generation"
        );

        assert_eq!(chunk_seq.load(Ordering::Acquire), 1);
        assert_eq!(
            settled_chunk_seq.load(Ordering::Acquire),
            1,
            "a discarded pre-seed read must be settled before capture"
        );

        let parser_guard = parser.lock().unwrap();
        seeded.store(true, Ordering::Release);
        conn.write_all(b"POST-SEED-OUTPUT")
            .expect("write post-seed");
        let deadline = Instant::now() + Duration::from_secs(5);
        while chunk_seq.load(Ordering::Acquire) < 2 {
            assert!(
                Instant::now() < deadline,
                "reader did not fence queued chunk"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(grid_gen.load(Ordering::Relaxed), 0);
        assert_eq!(
            settled_chunk_seq.load(Ordering::Acquire),
            1,
            "a queued chunk must remain unsettled until it mutates the parser"
        );
        assert!(
            snapshot.try_lock().is_err(),
            "the reader must hold the snapshot fence while waiting to parse"
        );
        let (swap_tx, swap_rx) = std::sync::mpsc::channel();
        let swap_parser = parser.clone();
        let swap_snapshot = snapshot.clone();
        let swap_stream = stream.clone();
        let swap_cursor = app_cursor.clone();
        let swap_grid_gen = grid_gen.clone();
        let swap_chunk_seq = chunk_seq.clone();
        let swap_settled_chunk_seq = settled_chunk_seq.clone();
        let swap = std::thread::spawn(move || {
            let _snapshot = swap_snapshot.lock().expect("reader fence");
            let pipe = swap_stream.lock().expect("socket clone");
            let result = swap_seeded_parser(
                SeedSink {
                    parser: &swap_parser,
                    app_cursor: &swap_cursor,
                    grid_gen: &swap_grid_gen,
                    links: &LinkTable::default(),
                },
                None,
                b"POST-SEED-OUTPUT\r\n",
                (40, 6),
                SeedGuard {
                    chunk: Some((&swap_chunk_seq, &swap_settled_chunk_seq, 1)),
                    pipe: pipe.as_ref(),
                },
            );
            swap_tx.send(result).expect("report seed result");
        });
        stop.store(true, Ordering::Relaxed);
        drop(parser_guard);
        assert_eq!(
            swap_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("seed result"),
            VtRefreshResult::Busy,
            "a snapshot must not replace the parser behind a received chunk"
        );
        swap.join().expect("snapshot exits");
        while settled_chunk_seq.load(Ordering::Acquire) < 2 {
            assert!(Instant::now() < deadline, "post-seed chunk never settled");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(grid_gen.load(Ordering::Relaxed), 1);

        let screen = {
            let p = parser.lock().unwrap();
            let s = p.screen();
            (0..6)
                .map(|r| {
                    (0..40)
                        .map(|c| match s.cell(r, c) {
                            Some(cell) if cell.has_contents() => cell.contents(),
                            _ => " ",
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(
            !screen.contains("PRE-SEED"),
            "pre-seed bytes were replayed into the grid (double-applied):\n{screen}"
        );
        assert!(
            screen.contains("POST-SEED-OUTPUT"),
            "post-seed bytes must still reach the grid:\n{screen}"
        );

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn reconcile_step_resizes_and_confirms_drift_before_capture_fallback() {
        let cases = [
            (
                (80, 24, 5, 3),
                (80, 24, 5, 3),
                None,
                7,
                GridReconcile::InSync,
            ),
            (
                (80, 24, 5, 3),
                (80, 24, 5, 3),
                Some(7),
                7,
                GridReconcile::InSync,
            ),
            (
                (80, 30, 5, 3),
                (80, 24, 9, 9),
                None,
                7,
                GridReconcile::Resize,
            ),
            (
                (81, 24, 5, 3),
                (80, 24, 5, 3),
                Some(7),
                7,
                GridReconcile::Resize,
            ),
            (
                (80, 24, 5, 3),
                (80, 24, 4, 3),
                None,
                7,
                GridReconcile::ArmDrift,
            ),
            (
                (80, 24, 5, 3),
                (80, 24, 4, 3),
                Some(7),
                8,
                GridReconcile::ArmDrift,
            ),
            (
                (80, 24, 5, 3),
                (80, 24, 4, 3),
                Some(7),
                7,
                GridReconcile::Reseed,
            ),
            (
                (80, 24, 5, 6),
                (80, 24, 5, 3),
                Some(0),
                0,
                GridReconcile::Reseed,
            ),
            (
                (10, 5, 10, 0),
                (10, 5, 9, 0),
                None,
                7,
                GridReconcile::InSync,
            ),
            (
                (10, 5, 10, 0),
                (10, 5, 9, 0),
                Some(7),
                7,
                GridReconcile::InSync,
            ),
            (
                (80, 24, 10, 0),
                (80, 24, 9, 0),
                Some(7),
                7,
                GridReconcile::Reseed,
            ),
        ];
        for (tmux, grid, pending, gen, want) in cases {
            assert_eq!(
                reconcile_step(tmux, grid, pending, gen),
                want,
                "tmux={tmux:?} grid={grid:?} pending={pending:?} gen={gen}"
            );
        }
    }

    #[test]
    fn parse_size_cursor_rejects_short_or_non_numeric_probes() {
        let cases = [
            ("80 24 5 3", Some((80u16, 24u16, 5u16, 3u16))),
            ("80 24 5 3\n", Some((80, 24, 5, 3))),
            ("80 24 5 3 99", Some((80, 24, 5, 3))),
            ("80 24 5", None),
            ("80 24", None),
            ("", None),
            ("80 24 5 #{cursor_y}", None),
            ("80 24 -1 3", None),
            ("80 24 5 99999", None),
        ];
        for (raw, want) in cases {
            assert_eq!(parse_size_cursor(raw), want, "{raw:?}");
        }
    }

    #[test]
    fn sync_output_scanner_tracks_2026_across_chunks_and_param_lists() {
        let mut sc = SyncOutputScanner::new();
        let mut out = Vec::new();
        let mut scan = |sc: &mut SyncOutputScanner, chunk: &[u8]| {
            out.clear();
            sc.feed(chunk, &mut out);
            out.clone()
        };
        assert!(scan(&mut sc, b"plain text \x1b[31m").is_empty());
        let opener = b"\x1b[?2026h";
        for (i, _) in opener.iter().enumerate().skip(1) {
            let mut split = SyncOutputScanner::new();
            assert!(scan(&mut split, &opener[..i]).is_empty());
            assert_eq!(scan(&mut split, &opener[i..]), vec![true], "split at {i}");
        }
        assert_eq!(scan(&mut sc, b"\x1b[?2026h"), vec![true]);
        assert_eq!(scan(&mut sc, b"\x1b[?25;2026l"), vec![false]);
        assert!(scan(&mut sc, b"\x1b[?1049h\x1b[?25l").is_empty());
        assert!(scan(&mut sc, b"\x1b[2026h").is_empty());
        assert_eq!(
            scan(&mut sc, b"\x1b[?2026h frame \x1b[?2026l"),
            vec![true, false]
        );
        assert_eq!(
            scan(&mut sc, b"tail \x1b[?2026l head \x1b[?2026h"),
            vec![false, true]
        );
    }

    #[test]
    fn sync_hold_plan_gives_each_bracket_its_own_lifetime() {
        for (events, want) in [
            (
                &[][..],
                SyncHoldPlan {
                    open: false,
                    restart: false,
                    close: false,
                },
            ),
            (
                &[true][..],
                SyncHoldPlan {
                    open: true,
                    restart: false,
                    close: false,
                },
            ),
            (
                &[false][..],
                SyncHoldPlan {
                    open: false,
                    restart: false,
                    close: true,
                },
            ),
            (
                &[true, false][..],
                SyncHoldPlan {
                    open: true,
                    restart: false,
                    close: true,
                },
            ),
            (
                &[false, true][..],
                SyncHoldPlan {
                    open: true,
                    restart: true,
                    close: false,
                },
            ),
            (
                &[false, true, false, true][..],
                SyncHoldPlan {
                    open: true,
                    restart: true,
                    close: false,
                },
            ),
        ] {
            assert_eq!(SyncHoldPlan::from_events(events), want, "{events:?}");
        }

        let signals = ViewerSignals::new();
        let stale = u64::MAX;
        signals.sync_hold_since_ms.store(stale, Ordering::Relaxed);
        SyncHoldPlan::from_events(&[true]).begin(&signals, || 100);
        assert_eq!(signals.sync_hold_since_ms.load(Ordering::Relaxed), stale);
        SyncHoldPlan::from_events(&[false, true]).begin(&signals, || 100);
        let fresh = signals.sync_hold_since_ms.load(Ordering::Relaxed);
        assert_ne!(fresh, stale, "a new bracket gets a new timestamp");
        assert_ne!(fresh, 0, "and the hold is never dropped between them");
    }

    #[test]
    fn viewer_signals_hold_opens_and_closes() {
        let signals = ViewerSignals::new();
        assert!(!signals.hold_active_at(100));
        signals.begin_hold(100);
        assert!(signals.hold_active_at(100));
        let since = signals.sync_hold_since_ms.load(Ordering::Relaxed);
        signals.begin_hold(101);
        assert_eq!(signals.sync_hold_since_ms.load(Ordering::Relaxed), since);
        signals.end_hold();
        assert!(!signals.hold_active_at(100));
        assert!(!signals.incomplete_within(100));

        signals.begin_hold(100);
        let since = signals.sync_hold_since_ms.load(Ordering::Relaxed);
        for (elapsed, hold, incomplete) in [
            (0, true, true),
            (SYNC_HOLD_MAX_MS - 1, true, true),
            (SYNC_HOLD_MAX_MS, false, true),
            (SYNC_BRACKET_ABANDON_MS - 1, false, true),
            (SYNC_BRACKET_ABANDON_MS, false, false),
        ] {
            let now = since + elapsed;
            assert_eq!(signals.hold_active_at(now), hold, "hold at {elapsed}ms");
            assert_eq!(
                signals.incomplete_within(now),
                incomplete,
                "incomplete at {elapsed}ms"
            );
        }
    }

    #[test]
    fn repeated_close_open_reads_cannot_freeze_the_view() {
        let signals = ViewerSignals::new();
        signals.begin_hold(100);
        let run_started = signals.incomplete_since_ms.load(Ordering::Relaxed);
        let mut bracket = signals.sync_hold_since_ms.load(Ordering::Relaxed);
        for read in 1..=50 {
            SyncHoldPlan::from_events(&[false, true]).begin(&signals, || 100 + read);
            let next = signals.sync_hold_since_ms.load(Ordering::Relaxed);
            assert!(next >= bracket, "read {read}: bracket hold moves forward");
            bracket = next;
            assert_eq!(
                signals.incomplete_since_ms.load(Ordering::Relaxed),
                run_started,
                "read {read}: an unsampled close does not extend the abandon window"
            );
        }
        assert!(
            !signals.incomplete_within(run_started + SYNC_BRACKET_ABANDON_MS),
            "the run still expires, so frames publish again"
        );

        SyncHoldPlan::from_events(&[false]).end(&signals);
        assert_eq!(signals.incomplete_since_ms.load(Ordering::Relaxed), 0);
        assert!(!signals.incomplete_within(100));
        signals.begin_hold(100);
        assert!(signals.incomplete_within(100));
        assert!(signals.hold_active_at(100));
    }

    #[test]
    fn a_restart_over_a_settled_grid_starts_the_incomplete_run() {
        let signals = ViewerSignals::new();
        assert_eq!(signals.incomplete_since_ms.load(Ordering::Relaxed), 0);
        let plan = SyncHoldPlan::from_events(&[true, false, true]);
        assert!(plan.restart, "the last opener follows a close");
        assert!(!plan.close, "and the read ends inside the new bracket");
        plan.begin(&signals, || 100);
        assert!(
            signals.incomplete_within(100),
            "the half-applied repaint is held"
        );
        assert!(signals.hold_active_at(100));
        assert_ne!(signals.incomplete_since_ms.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn reader_holds_viewer_wakeups_inside_a_synchronized_output_bracket() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let signals = Arc::new(ViewerSignals::new());
        let wakeup: ChangeWakeup = Arc::new((Mutex::new(0u64), Condvar::new()));
        let ctx = ReaderCtx {
            stop: stop.clone(),
            wakeup: Arc::new(Mutex::new(Some(wakeup.clone()))),
            grid_gen: grid_gen.clone(),
            signals: signals.clone(),
            ..ReaderCtx::for_test()
        };
        let rx = signals.changed_tx.subscribe();
        let parser = ctx.parser.clone();
        let settled = ctx.settled_chunk_seq.clone();
        let reader = std::thread::spawn(move || run_reader(listener, ctx, || 100));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        conn.write_all(b"\x1b[?2026h\x1b[2J\x1b[HPART-A")
            .expect("write");
        let deadline = Instant::now() + Duration::from_secs(5);
        while settled.load(Ordering::Acquire) < 1 {
            assert!(Instant::now() < deadline, "reader never parsed the chunk");
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(parser.lock().unwrap());
        assert!(
            signals.hold_active_at(100),
            "bracket opened: hold must be active"
        );
        assert!(
            !rx.has_changed().unwrap(),
            "no viewer wake inside the bracket"
        );
        assert_eq!(
            *wakeup.0.lock().unwrap(),
            0,
            "no poller wake inside the bracket"
        );

        conn.write_all(b"\x1b[5;1HPART-B\x1b[?2026l")
            .expect("write");
        while settled.load(Ordering::Acquire) < 2 {
            assert!(Instant::now() < deadline, "reader never parsed the close");
            std::thread::sleep(Duration::from_millis(2));
        }
        drop(parser.lock().unwrap());
        assert!(
            !signals.hold_active_at(100),
            "bracket closed: hold released"
        );
        assert!(
            rx.has_changed().unwrap(),
            "viewers wake when the frame completes"
        );
        assert_eq!(*wakeup.0.lock().unwrap(), 1);

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn reader_releases_a_bracket_closed_before_the_grid_is_seeded() {
        use std::io::Write;

        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let settled = Arc::new(AtomicU64::new(0));
        let signals = Arc::new(ViewerSignals::new());
        let ctx = ReaderCtx {
            stop: stop.clone(),
            seeded: Arc::new(AtomicBool::new(false)),
            settled_chunk_seq: settled.clone(),
            signals: signals.clone(),
            ..ReaderCtx::for_test()
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx, || 100));
        let mut conn = UnixStream::connect(&sock).expect("connect");
        let deadline = Instant::now() + Duration::from_secs(5);
        let await_chunk = |seq: u64| {
            while settled.load(Ordering::Acquire) < seq {
                assert!(
                    Instant::now() < deadline,
                    "reader never consumed chunk {seq}"
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        };

        conn.write_all(b"\x1b[?2026h\x1b[2JPART-A").expect("write");
        await_chunk(1);
        assert!(signals.hold_active_at(100), "pre-seed opener still holds");

        conn.write_all(b"PART-B\x1b[?2026l").expect("write");
        await_chunk(2);
        assert!(
            !signals.hold_active_at(100),
            "pre-seed close releases the hold"
        );
        assert!(!signals.incomplete_within(100));

        stop.store(true, Ordering::Relaxed);
        drop(conn);
        let _ = reader.join();
    }

    #[test]
    fn sample_serves_last_complete_frame_while_bracket_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel("aoe-vt-hold-test", dir.path());
        ch.parser.lock().unwrap().process(b"before");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        let first = ch.sample_with_clock(4, &deadline, || 100).content;
        assert!(first.contains("before"));

        ch.signals.begin_hold(100);
        ch.parser.lock().unwrap().process(b"\r\x1b[Kafter");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let held = ch.sample_with_clock(4, &deadline, || 100).content;
        assert_eq!(
            held, first,
            "mid-bracket sample serves the last complete frame"
        );

        ch.signals.end_hold();
        let fresh = ch.sample_with_clock(4, &deadline, || 100).content;
        assert!(
            fresh.contains("after"),
            "closing the bracket publishes the new frame"
        );
        assert!(!fresh.contains("before"));
    }

    #[test]
    fn a_resize_holds_every_viewer_off_the_grid_until_the_parser_catches_up() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel("aoe-vt-resync-test", dir.path());
        assert!(!ch.grid_resync_pending(), "a settled grid owes nothing");

        ch.expect_grid_size(40, 10);
        assert!(ch.grid_resync_pending());
        assert_eq!(ch.pending_resync_target(), Some((40, 10)));

        assert!(ch.grid_resync_pending());

        ch.cols.store(40, Ordering::Relaxed);
        ch.rows.store(10, Ordering::Relaxed);
        assert_eq!(ch.pending_resync_target(), None);
        assert!(!ch.grid_resync_pending());

        ch.begin_resize(80, 24).abandon();
        assert!(!ch.grid_resync_pending());
        let mine = ch.begin_resize(80, 24);
        let theirs = ch.begin_resize(100, 30);
        mine.abandon();
        assert_eq!(
            ch.pending_resync_target(),
            Some((100, 30)),
            "a superseded expectation must not clear the live one"
        );
        drop(theirs);

        let mine = ch.begin_resize(100, 30);
        let theirs = ch.begin_resize(100, 30);
        mine.abandon();
        assert_eq!(
            ch.pending_resync_target(),
            Some((100, 30)),
            "an identical declaration is still someone else's"
        );
        drop(theirs);
        assert!(
            ch.grid_resync_pending(),
            "and it outlives the resize window"
        );

        ch.cols.store(100, Ordering::Relaxed);
        ch.rows.store(30, Ordering::Relaxed);
        let owner = ch.begin_resize(132, 43);
        let follower = ch.begin_resize(132, 43);
        follower.abandon();
        assert_eq!(
            ch.pending_resync_target(),
            Some((132, 43)),
            "a live resize still owes its geometry after a later one withdraws"
        );
        drop(owner);
        ch.cols.store(40, Ordering::Relaxed);
        ch.rows.store(10, Ordering::Relaxed);
        ch.expect_grid_size(100, 30);

        let grid = (
            ch.cols.load(Ordering::Relaxed),
            ch.rows.load(Ordering::Relaxed),
        );
        ch.observe_pane_geometry(grid, ch.resize_observation());
        assert!(!ch.grid_resync_pending(), "an unmet request is dropped");

        ch.observe_pane_geometry((132, 43), ch.resize_observation());
        assert!(!ch.grid_resync_pending(), "reconcile opens no expectation");

        drop(ch.begin_resize(1, 1));
        ch.observe_pane_geometry((132, 43), ch.resize_observation());
        assert_eq!(ch.pending_resync_target(), Some((132, 43)));
        for _ in 0..10 {
            ch.expect_grid_size(132, 43);
            assert!(ch.grid_resync_pending(), "a live divergence stays gated");
        }
        ch.cols.store(132, Ordering::Relaxed);
        ch.rows.store(43, Ordering::Relaxed);
        assert!(!ch.grid_resync_pending(), "landing the reseed ends it");
    }

    #[test]
    fn a_geometry_probe_that_straddles_a_resize_cannot_retire_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel("aoe-vt-resize-race", dir.path());
        let settled = (
            ch.cols.load(Ordering::Relaxed),
            ch.rows.load(Ordering::Relaxed),
        );

        let in_flight = ch.begin_resize(100, 30);
        let probe = ch.resize_observation();
        drop(in_flight);

        ch.observe_pane_geometry(settled, probe);
        assert_eq!(
            ch.pending_resync_target(),
            Some((100, 30)),
            "a probe that straddled the resize must not retire it"
        );

        let in_flight = ch.begin_resize(100, 30);
        let probe = ch.resize_observation();
        ch.observe_pane_geometry(settled, probe);
        assert!(ch.grid_resync_pending(), "nor one taken mid-resize");
        drop(in_flight);

        let old_owner = ch.begin_resize(100, 30);
        let new_owner = ch.begin_resize(100, 30);
        let probe = ch.resize_observation();
        ch.observe_pane_geometry(settled, probe);
        assert!(
            ch.grid_resync_pending(),
            "overlapping resizes must not read as none in flight"
        );
        drop(new_owner);
        old_owner.abandon();
        let probe = ch.resize_observation();
        ch.observe_pane_geometry((100, 30), probe);
        assert_eq!(
            ch.pending_resync_target(),
            Some((100, 30)),
            "a follower stays gated while the reseed still owes the geometry"
        );
        ch.cols.store(100, Ordering::Relaxed);
        ch.rows.store(30, Ordering::Relaxed);
        assert!(
            !ch.grid_resync_pending(),
            "a landed reseed resumes the grid"
        );

        ch.expect_grid_size(80, 24);
        let probe = ch.resize_observation();
        ch.observe_pane_geometry((100, 30), probe);
        assert!(
            !ch.grid_resync_pending(),
            "a quiescent probe still resolves a request the pane never took"
        );
    }

    #[test]
    fn sample_reports_a_mid_bracket_cache_miss_as_incomplete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel("aoe-vt-partial-test", dir.path());
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        ch.parser.lock().unwrap().process(b"whole");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let cached = ch.sample_with_clock(4, &deadline, || 100);
        assert!(cached.content.contains("whole"));
        assert!(!cached.incomplete);

        ch.signals.begin_hold(100);
        ch.parser.lock().unwrap().process(b"\r\x1b[Kpart");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);

        let hit = ch.sample_with_clock(4, &deadline, || 100);
        assert_eq!(hit.content, cached.content, "cache hit stays whole");
        assert!(!hit.incomplete);

        let miss = ch.sample_with_clock(3, &deadline, || 100);
        assert!(miss.content.contains("part"), "cache miss reassembles");
        assert!(miss.incomplete, "a mid-bracket assembly is not publishable");

        ch.signals.end_hold();
        assert!(miss.incomplete);

        let after = ch.sample_with_clock(3, &deadline, || 100);
        assert!(!after.incomplete, "a closed bracket publishes again");
        assert!(after.content.contains("part"));
    }
}
