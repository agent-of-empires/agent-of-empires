//! Shared in-process VT channel.
//!
//! A `tmux pipe-pane -IO` stream feeds a pane's raw output into an in-process
//! [`vt100::Parser`] (a real grid: alt-screen buffer, cursor, mouse/DEC modes),
//! and the same full-duplex unix socket carries keystroke bytes back to the
//! pane. tmux still owns the pane (process, persistence, kill-tree); only the
//! live render/input transport lives here.
//!
//! One [`VtChannel`] per tmux session, shared and refcounted by native live
//! previews. The channel tears down (disables the pipe, stops the forwarder)
//! when the last `Arc` drops. Unix-only; the whole module is `#[cfg(unix)]`.

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

/// Longest a DEC 2026 synchronized-output bracket suppresses viewer wakeups.
/// Past this the capture loop resumes its normal cadence so death detection
/// and the size-owner heartbeat keep running; the sampler still prefers the
/// last complete frame, so resuming costs no tearing.
const SYNC_HOLD_MAX_MS: u64 = 200;
/// Longest the sampler keeps preferring the last complete frame over a grid
/// that is still mid-bracket. A repaint slower than [`SYNC_HOLD_MAX_MS`] is
/// ordinary on a loaded machine and must not tear; an app that opens a bracket
/// and never closes it is stuck, and past this its partial screen is the only
/// truth left to show.
const SYNC_BRACKET_ABANDON_MS: u64 = 2_000;

#[derive(Clone, Copy, PartialEq)]
enum SyncState {
    Idle,
    Esc,
    Csi,
    Params,
}

/// Incremental detector for `CSI ? <params> h|l` carrying mode 2026 (DEC
/// synchronized output). Full-screen agents wrap each repaint in that bracket
/// so terminals paint it atomically; the reader uses it to hold viewer wakeups
/// until the frame is complete. Per-byte state survives chunk boundaries, and
/// 2026 is matched anywhere in a `;`-separated parameter list.
///
/// A parameter list longer than 32 bytes abandons the sequence rather than
/// growing the buffer, so a pane cannot make this allocate. Missing a bracket
/// costs only the hold for that repaint (the frame publishes as it does on the
/// capture path), and no real 2026 bracket is anywhere near that long: apps
/// emit it bare, and the whole point is that it is cheap to write per frame.
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

    /// Scan one chunk; returns the last 2026 transition it contains
    /// (`Some(true)` = bracket opened, `Some(false)` = closed).
    fn feed(&mut self, chunk: &[u8]) -> Option<bool> {
        use SyncState::*;
        let mut last = None;
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
                        last = Some(b == b'h');
                    }
                    Idle
                }
                (Esc | Csi | Params, 0x1b) => Esc,
                (Esc | Csi | Params, _) => Idle,
            };
        }
        last
    }
}

/// Signals the reader thread raises for out-of-process-loop viewers (the web
/// live view): a watch that bumps on every publishable grid change, a
/// non-consuming clipboard slot with a sequence so several viewers can each
/// see one OSC 52 write, and the synchronized-output hold that keeps a
/// half-drawn frame from being sampled.
pub(crate) struct ViewerSignals {
    changed_tx: tokio::sync::watch::Sender<()>,
    clipboard_latest: Mutex<Option<String>>,
    clipboard_seq: AtomicU64,
    /// Millis since `CHUNK_CLOCK` when the current 2026 bracket opened; 0 when
    /// no bracket is open.
    sync_hold_since_ms: AtomicU64,
}

impl ViewerSignals {
    fn new() -> Self {
        Self {
            changed_tx: tokio::sync::watch::channel(()).0,
            clipboard_latest: Mutex::new(None),
            clipboard_seq: AtomicU64::new(0),
            sync_hold_since_ms: AtomicU64::new(0),
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

    fn begin_hold(&self) {
        if self.sync_hold_since_ms.load(Ordering::Relaxed) == 0 {
            self.sync_hold_since_ms
                .store(chunk_now_ms().max(1), Ordering::Relaxed);
        }
    }

    fn end_hold(&self) {
        self.sync_hold_since_ms.store(0, Ordering::Relaxed);
    }

    /// True while a synchronized-output bracket is open and has not outlived
    /// [`SYNC_HOLD_MAX_MS`]. Gates wakeups and publication.
    pub(crate) fn hold_active(&self) -> bool {
        self.open_within(chunk_now_ms(), SYNC_HOLD_MAX_MS)
    }

    /// True while the grid holds a frame the app has not finished drawing, up
    /// to [`SYNC_BRACKET_ABANDON_MS`]. Outlives [`Self::hold_active`] so a slow
    /// repaint is served from the last complete frame instead of torn.
    pub(crate) fn frame_incomplete(&self) -> bool {
        self.open_within(chunk_now_ms(), SYNC_BRACKET_ABANDON_MS)
    }

    fn open_within(&self, now_ms: u64, window_ms: u64) -> bool {
        let since = self.sync_hold_since_ms.load(Ordering::Relaxed);
        since != 0 && now_ms.saturating_sub(since) < window_ms
    }
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

/// Per-session arm locks: concurrent `acquire`s for one session must not both
/// run `arm`, because the second `tmux pipe-pane` replaces the first's pipe,
/// and whichever channel then loses the registry race `Drop`s, disabling the
/// SURVIVOR's pipe and leaving the pane with no pipe at all. Serializing the
/// arm makes the loser wait and adopt the winner's live channel instead. Kept
/// separate from `REGISTRY`'s lock, which is taken on every keystroke and
/// must never wait out an arm (~500ms). Entries are pruned once no acquire
/// holds them, so the map tracks in-flight arms, not session history.
static ARM_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Read-only OSC 52 observers need the same in-process sharing as VT grids:
/// `pipe-pane` permits one command per pane, so two browser connections must
/// hold one observer rather than replacing each other's forwarder.
static OSC52_REGISTRY: LazyLock<Mutex<HashMap<String, Weak<Osc52Channel>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
static OSC52_ARM_LOCKS: LazyLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

static SOCK_COUNTER: AtomicU64 = AtomicU64::new(0);
static PIPE_OWNER_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Monotonic base for chunk-arrival timestamps. The reader stamps each chunk's
/// arrival against this (millis), and the TUI capture worker reads the deltas
/// via VtChannel::chunk_timing to drive its repaint-quiescence debounce.
static CHUNK_CLOCK: LazyLock<Instant> = LazyLock::new(Instant::now);

fn chunk_now_ms() -> u64 {
    CHUNK_CLOCK.elapsed().as_millis() as u64
}

/// Unique lease identity for one armed pipe generation. A process can retain
/// a dead channel while its replacement arms under the same session name, so
/// process identity alone cannot fence stale shutdown and heartbeat calls.
fn new_pipe_owner_id() -> String {
    format!(
        "pipe-{}-{}",
        std::process::id(),
        PIPE_OWNER_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A `(Mutex, Condvar)` pair an in-process poller parks on. Registered via
/// [`VtChannel::set_change_wakeup`]; the reader thread pokes it after every
/// grid change (and on death) so the poller samples the moment output lands
/// instead of after the remainder of a fixed poll interval.
pub(crate) type ChangeWakeup = Arc<(Mutex<u64>, Condvar)>;

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
        if let Ok(mut generation) = pair.0.lock() {
            *generation = generation.wrapping_add(1);
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

fn lookup_osc52(session: &str) -> Option<Arc<Osc52Channel>> {
    OSC52_REGISTRY
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

/// The pane's geometry AND cursor in one `display-message` fork:
/// `(pane_width, pane_height, cursor_x, cursor_y)`, the cursor 0-based in
/// visible-screen coordinates (the space `assemble_seed_stream`'s CUP uses).
///
/// Folded into the geometry probe rather than run as a second fork because
/// [`VtChannel::reconcile_grid`] needs both on the same once-a-second budget:
/// the cursor is its drift detector and the geometry is its resize trigger.
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

/// Parse the four whitespace-separated fields `pane_size_cursor` asks tmux for.
/// Split out of the fork so the failure modes are testable: a pane that vanished
/// mid-probe, or a tmux that could not resolve a format, yields a short or
/// non-numeric line, and this must report `None` rather than a partial tuple.
/// A half-read cursor would look like drift to `reconcile_step` and reseed the
/// grid once a second, which is the flicker the drift detector exists to avoid.
fn parse_size_cursor(raw: &str) -> Option<(u16, u16, u16, u16)> {
    let mut it = raw.split_whitespace();
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    let cx = it.next()?.parse().ok()?;
    let cy = it.next()?.parse().ok()?;
    Some((w, h, cx, cy))
}

/// What one [`VtChannel::reconcile_grid`] pass should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GridReconcile {
    /// Grid agrees with the pane; clear any armed drift.
    InSync,
    /// Geometry changed: adopt the new size and reseed.
    Resize,
    /// Cursor disagrees for the first time. Remember the generation it was seen
    /// at; a racing probe resolves itself by the next pass.
    ArmDrift,
    /// Cursor still disagrees a full pass later with no output in between, so
    /// the grid is genuinely diverged from tmux. Reseed.
    Reseed,
}

/// Decide what a reconcile pass does, given the pane as tmux reports it, the
/// grid's own geometry and cursor, the grid generation a drift was first armed
/// at (`pending`), and the current generation.
///
/// Geometry wins: a resize reseeds anyway, so there is no point ruling on a
/// cursor that the reflow is about to move.
///
/// The cursor check is the grid's resync for a pane that is being watched but
/// never resizes. `pipe-pane` is a one-way byte stream with no
/// acknowledgement, so any byte the grid misses (or applies twice) is a
/// permanent divergence, and before this the only reseed was on a size change:
/// a pane that never resized stayed wrong indefinitely.
///
/// Confirming across two passes is what keeps it from firing on a race. The
/// probe is a fork, so a pane that emits output between the grid's last applied
/// chunk and the probe legitimately reports a cursor the grid has not reached
/// yet. `grid_gen` is the discriminator: it is bumped by every parsed chunk, so
/// an unchanged generation across two passes a second apart means the grid took
/// no output, and pipe-pane delivers every byte tmux wrote, so the pane emitted
/// none either. A cursor that still disagrees under those conditions cannot be
/// explained by a race. Streaming output keeps bumping the generation and so
/// never reaches `Reseed`, which is what stops a busy full-screen agent from
/// reseeding (and flickering) once a second.
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
    // Compare the last column as one bucket. tmux reports `cursor_x ==
    // pane_width` while a wrap is pending, and so does the grid *while
    // streaming*, but the seed's absolute CUP goes through vt100's `set_pos`,
    // which clamps the column to `cols - 1`. A pane parked at a pending wrap
    // therefore reads as a drift that reseeding can never clear, so an
    // unclamped comparison reseeds every other pass for as long as the pane is
    // viewed. The cost is missing a genuine one-column drift at the right
    // edge, which the next chunk of output moves off that column anyway.
    let last_col = tw.saturating_sub(1);
    if (tcx.min(last_col), tcy) == (gcx.min(last_col), gcy) {
        return GridReconcile::InSync;
    }
    match pending {
        Some(gen) if gen == grid_gen => GridReconcile::Reseed,
        _ => GridReconcile::ArmDrift,
    }
}

/// The pane state a seed needs that `capture-pane -e` can't carry: the terminal
/// modes the wheel-forward / scroll logic keys off, plus the real cursor
/// position and DECTCEM (show/hide) flag. `capture-pane` returns cell text and
/// SGR only, so without these the seeded parser has default modes and its cursor
/// stranded wherever the last replayed glyph ended (issue #2902).
///
/// `PartialEq` is the seed's race guard: `capture_seed_snapshot` probes this
/// state before and after the `capture-pane` fork and retries while the two
/// disagree, so a pane that scrolled, moved its cursor, or flipped screens
/// mid-seed can't stamp a stale position into the fresh grid. `history_size`
/// and `pane_height` exist for that comparison alone, mirroring the drift
/// fields `merge_cursor_probes` trusts on the legacy capture path.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
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
    /// `#{keypad_cursor_flag}`: DECCKM (application cursor keys). Without
    /// this seed, a channel armed while an app is already in
    /// application-cursor mode (vim, a full-screen agent) encodes arrows as
    /// `ESC [ A` instead of `ESC O A` until the app happens to re-emit the
    /// mode, and arrow keys misbehave in the meantime.
    app_cursor: bool,
    /// `#{history_size}`: scroll detector for the pre/post agreement check. A
    /// pane that scrolled between the probes grew its history, even when the
    /// cursor stayed pinned to the same bottom row.
    history_size: u32,
    /// `#{pane_height}`: resize detector for the same check; a resize mid-seed
    /// invalidates the coordinate space `cursor_y` was reported in.
    pane_height: u16,
    /// `#{pane_width}`: the other resize axis. A width-only resize rewraps the
    /// pane content, so a body captured before it pairs with stale geometry
    /// even when height, history, and cursor all happen to compare equal.
    pane_width: u16,
}

/// The `display-message` format both seed probes share. Field order matches
/// [`parse_seed_state`].
const SEED_STATE_FMT: &str = "#{alternate_on} #{mouse_any_flag} #{mouse_sgr_flag} #{mouse_all_flag} #{cursor_x} #{cursor_y} #{cursor_flag} #{keypad_cursor_flag} #{history_size} #{pane_height} #{pane_width}";

/// Parse one [`SEED_STATE_FMT`] line. Missing or malformed fields fall back to
/// the same defaults the old single-probe parser used, so a truncated line
/// still yields a usable (if conservative) state.
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

/// Query the pane's seed state in one `display-message` round-trip (the live
/// path is fork-sensitive, #2822, so modes and cursor share a single call).
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
/// modes, cursor, and hide flag come from [`capture_seed_snapshot`] and are
/// woven into the byte stream by [`assemble_seed_stream`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VtRefreshResult {
    Refreshed,
    Busy,
    Failed,
}
fn refresh_commits_geometry(result: VtRefreshResult) -> bool {
    result == VtRefreshResult::Refreshed
}

fn seed_parser(
    target: &str,
    parser: &Mutex<vt100::Parser>,
    app_cursor: &AtomicBool,
    grid_gen: &AtomicU64,
    size: (u16, u16),
    deadline: &crate::tmux::TmuxCommandDeadline,
    chunk_guard: Option<(&AtomicU64, &AtomicU64, u64)>,
) -> VtRefreshResult {
    let (_, rows) = size;
    let Some(stream) = capture_seed_stream(target, rows, deadline) else {
        return VtRefreshResult::Failed;
    };
    swap_seeded_parser(
        parser,
        app_cursor,
        grid_gen,
        None,
        &stream,
        size,
        chunk_guard,
    )
}
/// Capture the pane and weave its modes and cursor into one replayable byte
/// stream, or `None` when the pane could not be captured. Split from the swap
/// so a caller can bracket the (forking, multi-millisecond) capture with the
/// generation check `swap_seeded_parser` needs.
fn capture_seed_stream(
    target: &str,
    rows: u16,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> Option<Vec<u8>> {
    let (body, state) = capture_seed_snapshot(target, deadline)?;
    Some(assemble_seed_stream(&body, &state, rows))
}

/// Replace `parser` with a fresh grid built from `stream`, unless the reader
/// applied a chunk since generation `since` or has not settled the expected
/// chunk sequence.
///
/// The guards are the ordering boundary between the snapshot and `pipe-pane`
/// consumption (#3617). `capture_seed_stream` forks tmux, so `run_reader` can
/// take the parser lock first and apply a chunk that the snapshot does not
/// contain; replacing the parser would then drop that chunk from both grids.
/// Generation changes fence applied chunks, while the received/settled pair
/// also fences a chunk queued on this parser lock.
///
/// A raced swap is abandoned rather than retried inline: the old parser holds
/// the newer output, so leaving it alone is the safe side, and the caller
/// reseeds again on its own cadence. `since` of `None` disables only the
/// generation guard for callers whose current grid is stale by definition.
fn swap_seeded_parser(
    parser: &Mutex<vt100::Parser>,
    app_cursor: &AtomicBool,
    grid_gen: &AtomicU64,
    since: Option<u64>,
    stream: &[u8],
    size: (u16, u16),
    chunk_guard: Option<(&AtomicU64, &AtomicU64, u64)>,
) -> VtRefreshResult {
    let Ok(mut p) = parser.lock() else {
        return VtRefreshResult::Failed;
    };
    if since.is_some_and(|generation| generation != grid_gen.load(Ordering::Relaxed))
        || chunk_guard.is_some_and(|(received, settled, expected)| {
            received.load(Ordering::Acquire) != expected
                || settled.load(Ordering::Acquire) != expected
        })
    {
        return VtRefreshResult::Busy;
    }
    let (cols, rows) = size;
    *p = vt100::Parser::new(rows, cols, SCROLLBACK_LINES);
    p.process(stream);
    app_cursor.store(p.screen().application_cursor(), Ordering::Relaxed);
    grid_gen.fetch_add(1, Ordering::Relaxed);
    VtRefreshResult::Refreshed
}
/// How many times [`capture_seed_snapshot`] re-runs the probe/capture/probe
/// round before settling for its last (possibly raced) snapshot. Each retry
/// costs two forks plus a short settle sleep, and only fires while the pane is
/// actively changing under the seed, so the bound is about capping seed latency
/// on a pane that streams continuously, not about a steady state.
const SEED_PROBE_ATTEMPTS: usize = 3;

/// Pause between disagreeing seed attempts, letting a mid-flight burst (a
/// clear-then-reprint, an alt-screen flip) finish before the re-probe.
const SEED_RETRY_SETTLE: Duration = Duration::from_millis(5);

/// How many times arming re-runs the whole snapshot-and-install cycle when the
/// install fences against a chunk that landed during the capture. A busy pane
/// loses that race often; a handful of attempts finds a gap between repaints.
const SEED_INSTALL_ATTEMPTS: usize = 8;
/// Pause between those attempts. Long enough to clear a repaint burst, short
/// enough that eight of them stay well inside the tmux command deadline.
const SEED_INSTALL_RETRY: Duration = Duration::from_millis(20);

/// One `capture-pane -e` body plus a [`PaneSeedState`] that is KNOWN to
/// describe the same instant, or `None` when the pane is gone.
///
/// The state probe and the capture are separate tmux commands, and tmux
/// processes pane output between them: a seed taken while the pane streams,
/// clears, or flips the alternate screen would otherwise pair a stale cursor
/// (or screen-mode prefix) with newer cells, and `cursor_from_screen` stamps
/// the seeded position `position_reliable`, so the misplaced caret sticks
/// until the next output chunk moves it (the legacy capture path documents
/// this same race at ~100% of frames against a fast-scrolling pane, which is
/// why `capture_pane_with_cursor` double-probes). Guard the seed the same way:
/// probe, then run the capture and a second probe in ONE tmux invocation, and
/// accept the snapshot only when the two probes agree. On disagreement retry
/// after a short settle; a pane still changing after the last attempt seeds
/// from the final snapshot (its post-probe rode the same fork as the capture,
/// so it is the tightest pairing available, and the next live chunk heals any
/// residue).
fn capture_seed_snapshot(
    target: &str,
    deadline: &crate::tmux::TmuxCommandDeadline,
) -> Option<(Vec<u8>, PaneSeedState)> {
    let seed_start = format!("-{SCROLLBACK_LINES}");
    let mut last: Option<(Vec<u8>, PaneSeedState)> = None;
    for attempt in 0..SEED_PROBE_ATTEMPTS {
        if attempt > 0 {
            std::thread::sleep(SEED_RETRY_SETTLE);
        }
        // A failure mid-retry (pane vanished, fork error, half-run chain)
        // breaks to the tail rather than discarding an earlier attempt's
        // snapshot: every `last` is a self-consistent (body, probe) pair, and
        // seeding from it beats leaving the grid blank.
        let Some(pre) = pane_seed_state(target, deadline) else {
            break;
        };
        // The alternate screen has no scrollback, so only the normal buffer
        // pulls history (`-S`); the pane keeps that history across re-arms.
        // `-N` keeps trailing bg-styled fills (a modal backdrop painted as
        // full-width styled spaces) so the seeded grid renders them the same
        // way live chunks do (#3336).
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
        let Ok(out) = deadline.run(&mut command) else {
            break;
        };
        if !out.status.success() {
            break;
        }
        let (body, probe_line) = split_seed_capture(&out.stdout);
        // A chained invocation can exit 0 with the display-message half
        // silently dropped (the pane died between the sub-commands), leaving
        // the capture's last row where the probe belongs; feeding pane content
        // into the state parser would fabricate modes and a cursor.
        if !is_probe_line(probe_line) {
            break;
        }
        let post = parse_seed_state(probe_line);
        let agreed = pre == post;
        last = Some((body.to_vec(), post));
        if agreed {
            return last;
        }
    }
    if last.is_some() {
        tracing::debug!(
            %target,
            attempts = SEED_PROBE_ATTEMPTS,
            "vt seed: bracketing probes never agreed; seeding from last snapshot"
        );
    }
    last
}

/// Whether a chained-output line is plausibly the [`SEED_STATE_FMT`] probe
/// rather than a swallowed capture row: the probe's exact field count, every
/// token numeric. Guards the one hole in the chained transport, verified
/// against tmux 3.6: the invocation exits 0 even when its `display-message`
/// half silently fails (the pane died between the sub-commands), so status
/// alone cannot prove the probe line is present.
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

/// Split a chained `capture-pane ; display-message` output into the capture
/// body and the trailing probe line. The probe is the LAST line; everything
/// before it (including its own trailing newline, which
/// [`strip_trailing_row_terminator`] later drops) is the verbatim capture
/// body, so blank padded rows survive the split byte-for-byte.
fn split_seed_capture(raw: &[u8]) -> (&[u8], &str) {
    let trimmed = raw.strip_suffix(b"\n").unwrap_or(raw);
    match trimmed.iter().rposition(|&b| b == b'\n') {
        Some(idx) => (
            &trimmed[..=idx],
            std::str::from_utf8(&trimmed[idx + 1..]).unwrap_or(""),
        ),
        // Single line: no body, just the probe.
        None => (b"", std::str::from_utf8(trimmed).unwrap_or("")),
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
    // DECCKM: seed application-cursor mode so arrow keys encode correctly
    // (`ESC O A`) from the first keystroke after arming, instead of waiting
    // for the app to re-emit the mode. `seed_parser` reads the resulting
    // `application_cursor()` off the parser into the channel's `app_cursor`,
    // which is what the input path keys off.
    if state.app_cursor {
        out.extend_from_slice(b"\x1b[?1h");
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
fn tmux_supports_pipe_pane_io(deadline: &crate::tmux::TmuxCommandDeadline) -> bool {
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cached_tmux_support(&SUPPORTED, || {
        let mut command = crate::tmux::tmux_command();
        command.arg("-V");
        let out = deadline.run(&mut command).ok()?;
        if !out.status.success() {
            return None;
        }
        parse_tmux_pipe_support(&String::from_utf8_lossy(&out.stdout))
    })
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
    let digits: String = version
        .trim()
        .trim_start_matches(|c: char| !c.is_ascii_digit())
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let mut parts = digits.split('.');
    let major: u32 = parts.next()?.parse().ok()?;
    let minor: u32 = parts.next()?.parse().ok()?;
    Some((major, minor) >= (3, 4))
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
        // The grid is pane 0's alone. `capture_composited_over_grid` sets this
        // when it splices that grid into a composited window.
        composite_pane0: None,
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
    let last = row_last_col(screen, row, cols);
    row_to_ansi_upto(screen, row, last)
}

/// Columns of `row` that carry content, i.e. the trim point past which only
/// *unstyled* blank cells remain. Mirrors `capture-pane`'s trailing-space trim
/// so a row never carries a full width of padding into ratatui's wrapper. A
/// trailing blank that carries styling (a background fill running to the edge)
/// counts as content: it is drawn as a coloured space, exactly as a mid-row
/// styled blank already is.
///
/// The count is in display COLUMNS, not cells, so a trailing wide glyph
/// contributes both of the columns it occupies. Its continuation cell carries no
/// contents and, unstyled, no style either, so advancing by one per occupied
/// cell would under-count by one and leave
/// [`capture_rows_padded`] appending a space to a row that already fills its
/// pane, shifting every pane to its right by a column.
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

/// Serialise columns `0..last` of `row`. Split out of [`row_to_ansi`] so the
/// pane compositor can ask for a row rendered to its pane's full width rather
/// than to the trim point.
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

/// Render `raw` (one pane's `capture-pane -e -p` output) as exactly `rows`
/// ANSI rows, each padded with spaces to `cols` display columns.
///
/// The compositor splices panes side by side by *concatenating* their rows, so
/// unlike the single-pane preview path every row must occupy its pane's full
/// width: a trimmed row would let the next pane's first column slide left into
/// the gap. Going through a `vt100::Parser` rather than splitting the bytes on
/// newlines is what makes that safe, because a row's escape sequences are
/// resolved into cells before they are re-serialised, so no SGR state can leak
/// across a pane boundary into its neighbour.
pub(crate) fn capture_rows_padded(raw: &[u8], cols: u16, rows: u16) -> Vec<String> {
    let cols = cols.max(1);
    let rows = rows.max(1);
    // Parse at two rows minimum, then read back only the pane's real height.
    // vt100 0.16 underflows (panics) whenever content wraps on a ONE-row grid,
    // regardless of scrollback, and `resize-pane -y 1` makes that a layout a
    // user can actually produce. Captured content is already wrapped to the
    // pane's width so it normally fits exactly; this keeps a stale geometry
    // (the pane resized between the probe and the capture) from taking down
    // the render thread.
    let mut parser = vt100::Parser::new(rows.max(2), cols, 0);
    // `capture-pane` joins rows with a bare LF, which staircases each row off
    // the previous one's end column unless it is promoted to CRLF first (the
    // same seeding fix the live channel applies).
    parser.process(&lf_to_crlf(strip_trailing_row_terminator(raw)));

    let screen = parser.screen();
    (0..rows)
        .map(|row| {
            let last = row_last_col(screen, row, cols);
            let mut out = row_to_ansi_upto(screen, row, last);
            if last < cols {
                // Reset before padding so a styled final cell (a background
                // fill) does not bleed its colour across the gap.
                out.push_str("\x1b[0m");
                out.extend(std::iter::repeat_n(' ', (cols - last) as usize));
            }
            out
        })
        .collect()
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
    /// Read sequences no longer waiting to mutate the parser. A seed may
    /// commit only when this equals its arrival baseline.
    settled_chunk_seq: Arc<AtomicU64>,
    last_chunk_ms: Arc<AtomicU64>,
    prev_gap_ms: Arc<AtomicU64>,
    /// Grid generation, bumped after every parsed chunk so `sample`'s
    /// assembly cache invalidates the moment the grid could differ. Distinct
    /// from `chunk_seq`: re-seeds bump this too, and the debounce's
    /// first-chunk special case must not see seed bumps.
    grid_gen: Arc<AtomicU64>,
    signals: Arc<ViewerSignals>,
}

impl ReaderCtx {
    /// Wake the in-process poller and every watch subscriber.
    fn notify_viewers(&self) {
        notify_change_wakeup(&self.wakeup);
        self.signals.bump_changed();
    }
}

fn stop_and_wake_reader(stop: &AtomicBool, sock_path: &std::path::Path) {
    stop.store(true, Ordering::Relaxed);
    let _ = UnixStream::connect(sock_path);
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
    let mut sync = SyncOutputScanner::new();
    while !ctx.stop.load(Ordering::Relaxed) {
        match conn.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                // Track the app's synchronized-output bracket before anything
                // can publish this chunk: a frame is published when the
                // bracket closes (or the hold expires), never in the middle.
                let sync_event = sync.feed(&buf[..n]);
                // Opening is raised before the grid is touched, so a sampler
                // racing this chunk errs toward the last complete frame.
                // Closing is raised below, under the parser lock, because the
                // grid does not hold the finished frame until the chunk has
                // been applied.
                if sync_event == Some(true) {
                    ctx.signals.begin_hold();
                }
                // The vt100 parser below silently drops OSC 52, and in
                // live-send no tmux client is attached for `set-clipboard`
                // to forward to, so this tap is the ONLY path an agent's
                // copy has to the host clipboard (#2420). It is independent
                // of grid state, so it runs on every chunk, including the
                // pre-seed ones dropped just below: a copy that lands while
                // the channel is arming has no other route to the host.
                let copied = osc52.feed(&buf[..n]);
                if let Some(text) = copied.as_ref() {
                    if let Ok(mut guard) = ctx.clipboard.lock() {
                        *guard = Some(text.clone());
                    }
                    ctx.signals.publish_clipboard(text);
                }
                // Claim every read before waiting on the parser. An
                // authoritative seed that captured this output must then see
                // the changed sequence and return Busy instead of installing a
                // snapshot ahead of a queued chunk and applying it twice.
                let seq = ctx.chunk_seq.fetch_add(1, Ordering::AcqRel);
                // The initial snapshot is taken only after `seeded` flips.
                // Bytes received during the shorter pipe-connect window are
                // already present in that later snapshot, so do not replay them.
                if !ctx.seeded.load(Ordering::Acquire) {
                    ctx.settled_chunk_seq.store(seq + 1, Ordering::Release);
                    // OSC 52 remains independent of grid publication.
                    if copied.is_some() {
                        ctx.notify_viewers();
                    }
                    continue;
                }
                if let Ok(mut p) = ctx.parser.lock() {
                    p.process(&buf[..n]);
                    ctx.app_cursor
                        .store(p.screen().application_cursor(), Ordering::Relaxed);
                    // Bump while still holding the parser lock. A woken sampler
                    // sees the new generation, and a guarded seed swap cannot
                    // discard a chunk behind a generation bump that has not landed.
                    ctx.grid_gen.fetch_add(1, Ordering::Relaxed);
                    // Stamp this chunk's arrival so the capture worker can tell a
                    // lone chunk from a back-to-back stream and wait for settling.
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
                    // The finished frame is in the grid now, so the bracket can
                    // release; a sampler waiting on this lock sees a whole frame.
                    if sync_event == Some(false) {
                        ctx.signals.end_hold();
                    }
                    // Publish settlement after parser, cursor, generation, and
                    // timing updates. Acquire readers use this completion fence.
                    ctx.settled_chunk_seq.store(seq + 1, Ordering::Release);
                    // Inside a synchronized-output bracket the grid is a
                    // half-drawn frame; viewers wake when it closes.
                    if sync_event == Some(false) || !ctx.signals.hold_active() {
                        ctx.notify_viewers();
                    }
                }
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
    ctx.signals.end_hold();
    ctx.notify_viewers();
}

/// One shared pane channel: a vt100 grid fed by a `pipe-pane -IO` byte stream,
/// plus the writable half of the same socket for keystroke injection. Methods
/// take `&self` (interior mutability) so many viewers share one `Arc`.
pub(crate) struct VtChannel {
    /// tmux session name; the registry key.
    name: String,
    /// Fencing token for this exact pipe generation.
    owner_id: String,
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
    /// Highest contiguous read sequence that has either been applied to the
    /// parser or deliberately discarded before the initial snapshot.
    settled_chunk_seq: Arc<AtomicU64>,
    /// Arrival of the most recent chunk (millis since `CHUNK_CLOCK`), stamped
    /// by the reader thread on every chunk.
    last_chunk_ms: Arc<AtomicU64>,
    /// Interval between the two most recent chunks (millis). Large when the
    /// latest chunk followed a quiet gap (a lone keystroke echo); small during
    /// a back-to-back stream (a multi-chunk repaint). The sample debounce keys
    /// off this to tell the two apart.
    prev_gap_ms: Arc<AtomicU64>,
    /// Grid generation (see the `ReaderCtx` field of the same name): the
    /// cache key half of `sample_cache`.
    grid_gen: Arc<AtomicU64>,
    /// The last assembled sample, keyed by (generation, window, size). Every
    /// viewer samples on a cadence, but an idle pane's grid doesn't change
    /// between chunks, so re-walking (scrollback + screen) into ANSI each
    /// cycle is pure waste; the deeper the user has scrolled, the bigger the
    /// waste. A hit clones the cached string instead.
    ///
    /// One entry, so viewers watching this pane at different window sizes (a
    /// TUI preview beside a web viewer, or one client reading scrollback)
    /// evict each other and each miss. That costs an assembly, and it also
    /// means the mid-bracket path below cannot always answer from a complete
    /// frame; the web loop's own pre-publish check is what guarantees a torn
    /// frame is never sent. Keyed per window rather than per viewer because
    /// the common case is one viewer, and a map would outlive the connections
    /// that populated it.
    sample_cache: Mutex<Option<SampleCache>>,
    /// Shared with the reader thread; see [`ViewerSignals`].
    signals: Arc<ViewerSignals>,
    /// When this channel armed; a fresh seed may have caught a repaint
    /// mid-flight, which viewers use to hold their opening frame briefly.
    armed_at: Instant,
    /// Owner-only (0700) directory holding `sock_path`; removed on drop.
    sock_dir: PathBuf,
    sock_path: PathBuf,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    cols: AtomicU16,
    rows: AtomicU16,
    last_size_check: Mutex<Instant>,
    /// Grid generation a cursor drift was first seen at, or `None` when the
    /// grid last agreed with the pane. `reconcile_grid` only reseeds when the
    /// same drift survives a pass with this generation unchanged, so a probe
    /// that merely raced the byte stream costs nothing (see `reconcile_step`).
    pending_drift: Mutex<Option<u64>>,
    /// When this process last refreshed the cross-process VT-owner heartbeat,
    /// so `sample` refreshes at a fraction of `VT_OWNER_TTL` instead of
    /// forking `set-option` every call.
    last_owner_hb: Mutex<Instant>,
}

/// One cached [`VtChannel::sample`] assembly, valid while the grid
/// generation, requested window, and grid size all match.
struct SampleCache {
    grid_gen: u64,
    max_lines: usize,
    cols: u16,
    rows: u16,
    content: String,
    cursor: PaneCursor,
}

impl VtChannel {
    /// Get the shared channel for `session`, arming a new one if none is live.
    /// Returns `None` if tmux is too old or the pane is gone or any tmux/socket
    /// step fails; callers then use the legacy capture/send-keys path. The
    /// returned `Arc` keeps the channel alive; drop it to release this viewer's
    /// hold (the channel tears down when the last `Arc` drops).
    #[cfg(test)]
    pub(crate) fn acquire(session: &str) -> Option<Arc<VtChannel>> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        Self::acquire_with_deadline(session, &deadline)
    }

    pub(crate) fn acquire_with_deadline(
        session: &str,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<Arc<VtChannel>> {
        // Only a LIVE registry entry is reusable. A dead one (its pane was
        // killed and the tmux session recreated under the same name, e.g. a
        // session restart) must not be handed out: a viewer that received it
        // would sit on the capture fallback forever, and by holding the Arc
        // it would keep the corpse registered for the next viewer to trip
        // over. Arming fresh replaces the registry entry; the dead channel's
        // Drop leaves the replacement alone (its own weak no longer
        // upgrades).
        if let Some(ch) = lookup(session).filter(|c| c.is_alive()) {
            return Some(ch);
        }
        // Serialize arming per session: take (or create) this session's arm
        // lock, then re-check the registry under it, so the loser of a
        // concurrent race adopts the winner's channel instead of arming a
        // second pipe over it. The REGISTRY lock stays out of this: it is
        // taken on every keystroke and must never wait out an arm (~500ms).
        let arm_lock = ARM_LOCKS
            .lock()
            .unwrap()
            .entry(session.to_string())
            .or_default()
            .clone();
        let result = {
            let _armed = arm_lock.lock().unwrap();
            if let Some(ch) = lookup(session).filter(|c| c.is_alive()) {
                Some(ch)
            } else {
                // No `?` here: an arm failure must still fall through to the
                // prune below, or failed sessions would pile up in ARM_LOCKS.
                Self::arm(session, deadline).map(|ch| {
                    let ch = Arc::new(ch);
                    // With arms serialized, no live competitor can exist
                    // here; insert replaces at most a dead entry (whose Drop
                    // leaves the replacement alone, since its own weak no
                    // longer upgrades).
                    REGISTRY
                        .lock()
                        .unwrap()
                        .insert(session.to_string(), Arc::downgrade(&ch));
                    ch
                })
            }
        };
        // Drop finished arm locks so the map tracks in-flight arms only. Our
        // own entry survives while another acquire holds a clone (count > 1
        // besides the map's).
        drop(arm_lock);
        ARM_LOCKS
            .lock()
            .unwrap()
            .retain(|_, l| Arc::strong_count(l) > 1);
        result
    }

    fn arm(name: &str, deadline: &crate::tmux::TmuxCommandDeadline) -> Option<Self> {
        if !tmux_supports_pipe_pane_io(deadline) {
            return None;
        }
        let target = format!("{name}:^.0");
        // Arming only needs the geometry; the cursor rides along because the
        // probe is shared with `reconcile_grid` and costs one fork either way.
        let (cols, rows, _, _) = pane_size_cursor(&target, deadline)?;
        // `pipe-pane` is exclusive per pane: arming replaces (and thereby
        // kills) any other process's forwarder. Two aoe processes viewing the
        // same pane (a second TUI, the serve daemon's web live view) used to
        // fight over it on their re-arm throttles, flipping each other back
        // to the capture fallback every few seconds. Claim the cross-process
        // VT-owner lock first and defer if another live owner holds it; the
        // caller's capture fallback is fully functional, and the arm throttle
        // re-checks the lock so ownership transfers once the holder releases
        // (or its heartbeat goes stale: crash, kill -9).
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
        let stream: Arc<Mutex<Option<UnixStream>>> = Arc::new(Mutex::new(None));
        let app_cursor = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(false));
        let clipboard: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
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
        let wakeup: Arc<Mutex<Option<ChangeWakeup>>> = Arc::new(Mutex::new(None));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let last_chunk_ms = Arc::new(AtomicU64::new(0));
        let prev_gap_ms = Arc::new(AtomicU64::new(u64::MAX));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let signals = Arc::new(ViewerSignals::new());
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
                settled_chunk_seq: settled_chunk_seq.clone(),
                last_chunk_ms: last_chunk_ms.clone(),
                prev_gap_ms: prev_gap_ms.clone(),
                grid_gen: grid_gen.clone(),
                signals: signals.clone(),
            };
            std::thread::spawn(move || run_reader(listener, ctx))
        };

        let pipe_cmd = format!(
            "{} __vt-pipe {}",
            sh_quote(&exe.to_string_lossy()),
            sh_quote(&sock_path.to_string_lossy())
        );
        let armed = session.arm_vt_pipe_if_owner_with_deadline(&owner, "-IO", &pipe_cmd, deadline);
        if !armed {
            tracing::warn!(%target, "vt: tmux pipe-pane failed; falling back to capture");
            stop_and_wake_reader(&stop, &sock_path);
            // Free the owner lock we claimed above so another process can arm
            // right away instead of waiting out the TTL on our failed attempt.
            session.release_vt_pipe_owner_with_deadline(&owner, deadline);
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
                stop_and_wake_reader(&stop, &sock_path);
                session.release_vt_pipe_owner_with_deadline(&owner, deadline);
                let _ = reader.join();
                let _ = std::fs::remove_dir_all(&sock_dir);
                return None;
            }
            std::thread::sleep(Duration::from_millis(2));
        }

        // Mark the reader live before capture. Chunks observed before this point
        // are represented by the later snapshot; chunks observed after it are
        // applied to the parser and advance the guard before waiting on its lock.
        // The seed therefore either includes each chunk or returns Busy, never
        // dropping the capture-to-install window.
        seeded.store(true, Ordering::Release);
        // A pane that repaints continuously lands a chunk inside nearly every
        // seed window, and the fence then reports Busy rather than installing
        // a snapshot that would drop it. That is the pane being active, not
        // unseedable, so retry: giving up here strands the caller on the
        // capture fallback for the channel's whole lifetime, and a full-screen
        // agent is repainting from the moment it starts. Failed is different
        // and terminal (the pane is gone), so it breaks out immediately.
        let mut seed_result = VtRefreshResult::Failed;
        for attempt in 0..SEED_INSTALL_ATTEMPTS {
            if attempt > 0 {
                std::thread::sleep(SEED_INSTALL_RETRY);
            }
            let expected_chunk_seq = chunk_seq.load(Ordering::Acquire);
            seed_result = seed_parser(
                &target,
                &parser,
                &app_cursor,
                &grid_gen,
                (cols, rows),
                deadline,
                Some((&chunk_seq, &settled_chunk_seq, expected_chunk_seq)),
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
            session.release_vt_pipe_owner_with_deadline(&owner, deadline);
            let _ = reader.join();
            let _ = std::fs::remove_dir_all(&sock_dir);
            return None;
        }
        tracing::info!(
            %target,
            cols,
            rows,
            pid = std::process::id(),
            "vt channel armed (pipe-pane -IO <-> vt100 grid)"
        );

        Some(Self {
            name: name.to_string(),
            owner_id: owner,
            target,
            parser,
            stream,
            app_cursor,
            alive,
            wakeup,
            clipboard,
            chunk_seq,
            settled_chunk_seq,
            last_chunk_ms,
            prev_gap_ms,
            grid_gen,
            sample_cache: Mutex::new(None),
            signals,
            armed_at: Instant::now(),
            sock_dir,
            sock_path,
            stop,
            reader: Mutex::new(Some(reader)),
            cols: AtomicU16::new(cols),
            rows: AtomicU16::new(rows),
            last_size_check: Mutex::new(Instant::now()),
            pending_drift: Mutex::new(None),
            last_owner_hb: Mutex::new(Instant::now()),
        })
    }

    /// Keep the cross-process VT-owner heartbeat fresh while this channel is
    /// held. Every viewer's capture loop samples at least at idle cadence, so
    /// routing the refresh through `sample` keeps the lock alive exactly as
    /// long as someone is actually viewing through the pipe; a crashed
    /// process stops refreshing and the lock goes stale within
    /// `VT_OWNER_TTL`. Rate-limited to a fraction of the TTL (one
    /// `set-option` fork); a lost lock needs no demote here, because the new
    /// owner's arm replaces our pipe and the reader's EOF death path already
    /// flips every consumer to the capture fallback.
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

    /// Reconcile the parser with the pane at most once a second (one
    /// `display-message` fork; rate-limited so it adds no periodic hitch).
    ///
    /// Two triggers, both ending in a reseed from `capture-pane` rather than a
    /// bare `set_size`, because tmux reflows on resize while pipe-pane carries
    /// no reflow redraw (see `seed_parser`):
    ///
    /// - **geometry changed**, the original trigger.
    /// - **the cursor drifted** and stayed drifted across a pass with no output
    ///   in between, which means the grid genuinely diverged from tmux:
    ///   `pipe-pane` is an unacknowledged one-way stream, so a missed or doubled
    ///   byte is permanent, and a pane that never resizes used to carry that
    ///   divergence forever. `reconcile_step` owns the race-vs-drift call, and
    ///   deliberately does not reseed while output is flowing. Cursor-clean
    ///   cell drift is repaired by the capture worker's guarded authoritative
    ///   refresh without adding a second resync protocol.
    fn reconcile_grid(&self, deadline: &crate::tmux::TmuxCommandDeadline) {
        let mut guard = self.last_size_check.lock().unwrap();
        if guard.elapsed() < Duration::from_secs(1) {
            return;
        }
        *guard = Instant::now();
        drop(guard);
        let Some((c, r, cx, cy)) = pane_size_cursor(&self.target, deadline) else {
            return;
        };
        let (gc, gr) = (
            self.cols.load(Ordering::Relaxed),
            self.rows.load(Ordering::Relaxed),
        );
        // Read the cursor and the generation under ONE parser lock, which is
        // also where `run_reader` bumps the generation: a cursor that already
        // reflects a chunk therefore cannot pair with a generation that does
        // not, which would read as drift-without-output and reseed for nothing.
        let Ok(p) = self.parser.lock() else {
            return;
        };
        let (gcy, gcx) = p.screen().cursor_position();
        // Read generation after cursor while holding the same parser lock used
        // by the reader's generation bump. A processed cursor cannot pair with
        // a generation from before that chunk.
        let grid_gen = self.grid_gen.load(Ordering::Relaxed);
        drop(p);
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

    /// Forget any armed cursor drift. A poisoned lock leaves the old value in
    /// place, which at worst costs one extra reconcile pass; the alternative is
    /// panicking the render thread over a display-only heuristic.
    fn clear_drift(&self) {
        if let Ok(mut guard) = self.pending_drift.lock() {
            *guard = None;
        }
    }

    /// Rebuild the grid from `capture-pane` and clear any armed drift after a
    /// successful swap.
    ///
    /// `guarded` makes the swap conditional on the generation sampled before
    /// the capture. Healing reseeds are guarded because the current grid owns
    /// any concurrent output; resize reseeds are not, because tmux has reflowed
    /// and made the pre-resize grid stale. Both paths retain the received and
    /// settled chunk fence so a queued chunk cannot be duplicated or dropped.
    fn reseed(
        &self,
        cols: u16,
        rows: u16,
        guarded: bool,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> VtRefreshResult {
        let since = guarded.then(|| self.grid_gen.load(Ordering::Relaxed));
        let expected_chunk_seq = self.chunk_seq.load(Ordering::Acquire);
        let Some(stream) = capture_seed_stream(&self.target, rows, deadline) else {
            return VtRefreshResult::Failed;
        };
        let result = swap_seeded_parser(
            &self.parser,
            &self.app_cursor,
            &self.grid_gen,
            since,
            &stream,
            (cols, rows),
            Some((&self.chunk_seq, &self.settled_chunk_seq, expected_chunk_seq)),
        );
        if result == VtRefreshResult::Refreshed {
            self.clear_drift();
        }
        result
    }

    /// Rebuild from an authoritative tmux snapshot even when cursor and
    /// geometry probes agree, healing cell drift that those probes cannot see.
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
    /// Serialise up to max_lines of (scrollback + screen) to per-row ANSI,
    /// plus the authoritative cursor (with history_size set to the full
    /// scrollback depth). `max_lines` mirrors the capture path's window: both
    /// the TUI scroll and the web's virtual scroll spacer need real history
    /// here, not just the visible screen.
    #[cfg(test)]
    pub(crate) fn sample(&self, max_lines: usize) -> (String, Option<PaneCursor>) {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.sample_with_deadline(max_lines, &deadline)
    }

    pub(crate) fn sample_with_deadline(
        &self,
        max_lines: usize,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> (String, Option<PaneCursor>) {
        // Both fork tmux and take the parser lock themselves, so they run
        // before this sampler takes it.
        self.reconcile_grid(deadline);
        self.refresh_owner_heartbeat(deadline);
        let cols = self.cols.load(Ordering::Relaxed);
        let rows = self.rows.load(Ordering::Relaxed);
        let mut p = match self.parser.lock() {
            Ok(p) => p,
            Err(_) => return (String::new(), None),
        };
        // Read both under the parser lock, which is where the reader applies a
        // chunk and bumps the generation, and where it releases a bracket. The
        // grid therefore cannot change identity between these reads and the
        // assembly below.
        let grid_gen = self.grid_gen.load(Ordering::Relaxed);
        let incomplete = self.signals.frame_incomplete();
        if let Ok(guard) = self.sample_cache.lock() {
            if let Some(c) = guard.as_ref() {
                let same_window = (c.max_lines, c.cols, c.rows) == (max_lines, cols, rows);
                // Mid-bracket the grid is a half-drawn frame: serve the last
                // complete one instead. The reader wakes viewers on close.
                if same_window && (c.grid_gen == grid_gen || incomplete) {
                    return (c.content.clone(), Some(c.cursor));
                }
            }
        }
        let (content, history) = grid_content(&mut p, max_lines, cols, rows);
        let mut cursor = cursor_from_screen(p.screen(), rows, cols);
        cursor.history_size = history as u32;
        drop(p);
        // Never cache a frame assembled mid-bracket: it is half drawn, and a
        // cached copy would outlive the bracket that explains it.
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
        (content, Some(cursor))
    }

    /// Sample the VISIBLE grid as `want_rows` rows padded to `want_cols`
    /// display columns, for splicing this pane into a composited window.
    ///
    /// Unlike [`sample`](Self::sample) this never reaches into scrollback: a
    /// composite shows the live window only, since panes have independent
    /// histories with no coherent way to stack them.
    ///
    /// `want_cols` / `want_rows` come from tmux's view of the pane, which can
    /// briefly disagree with the grid mid-resize. Padding and truncating to the
    /// requested rectangle keeps that frame merely stale instead of shifting
    /// every pane to its right.
    #[cfg(test)]
    pub(crate) fn sample_rows_padded(
        &self,
        want_cols: u16,
        want_rows: u16,
    ) -> Option<(Vec<String>, PaneCursor)> {
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        self.sample_rows_padded_with_deadline(want_cols, want_rows, &deadline)
    }

    pub(crate) fn sample_rows_padded_with_deadline(
        &self,
        want_cols: u16,
        want_rows: u16,
        deadline: &crate::tmux::TmuxCommandDeadline,
    ) -> Option<(Vec<String>, PaneCursor)> {
        self.reconcile_grid(deadline);
        self.refresh_owner_heartbeat(deadline);
        let cols = self.cols.load(Ordering::Relaxed);
        let rows = self.rows.load(Ordering::Relaxed);
        let want_cols = want_cols.max(1);
        let want_rows = want_rows.max(1);

        let p = self.parser.lock().ok()?;
        let screen = p.screen();
        let readable_cols = cols.min(want_cols);
        let out = (0..want_rows)
            .map(|row| {
                if row >= rows {
                    // Grid shorter than tmux says the pane is: blank filler
                    // rather than a row borrowed from somewhere else.
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
        Some((out, cursor))
    }

    /// A receiver that fires on every publishable grid change, OSC 52 write, and
    /// on channel death. Each viewer holds its own so all of them wake;
    /// `changed()` also resolves at once when a bump landed since the last wait.
    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<()> {
        self.signals.changed_tx.subscribe()
    }

    /// Start a clipboard consumer at the current sequence, skipping writes
    /// that predate it (a newly opened viewer must not replay an old copy).
    pub(crate) fn clipboard_sequence(&self) -> u64 {
        self.signals.clipboard_seq.load(Ordering::Acquire)
    }

    /// The latest OSC 52 write after `seen`, advancing only this consumer's
    /// cursor. Non-consuming, unlike [`Self::take_clipboard`], so every viewer
    /// observes the event.
    pub(crate) fn clipboard_after(&self, seen: &mut u64) -> Option<String> {
        osc52_clipboard_after(
            &self.signals.clipboard_latest,
            &self.signals.clipboard_seq,
            seen,
        )
    }

    /// Re-sync the grid to a new pane size right after the size owner ran
    /// `resize-window`, instead of waiting for the periodic reconcile. Reseeds
    /// from `capture-pane` because tmux reflows on resize while `pipe-pane`
    /// carries no reflow redraw (see `seed_parser`).
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
        let result = self.reseed(cols, rows, false, deadline);
        if refresh_commits_geometry(result) {
            self.cols.store(cols, Ordering::Relaxed);
            self.rows.store(rows, Ordering::Relaxed);
            self.signals.bump_changed();
        }
        result
    }

    /// Time since this channel armed (and seeded from `capture-pane`).
    pub(crate) fn seed_age(&self) -> Duration {
        self.armed_at.elapsed()
    }

    /// Whether the pane is inside a synchronized-output bracket, i.e. the grid
    /// currently holds a frame the app has not finished drawing.
    pub(crate) fn sync_hold_active(&self) -> bool {
        self.signals.hold_active()
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

    fn write_input(&self, bytes: &[u8]) -> bool {
        use std::io::Write;
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

/// A raw `pipe-pane` reader used when a shell preview renders through
/// `capture-pane`. It observes OSC 52 writes without constructing a terminal
/// grid, so prompt redraws cannot affect the displayed frame.
pub(crate) struct Osc52Channel {
    name: String,
    /// Fencing token for this exact pipe generation.
    owner_id: String,
    clipboard: Arc<Mutex<Option<String>>>,
    /// Monotonically bumps after publishing a clipboard value. Consumers keep
    /// their own cursor so one dashboard viewer cannot consume an event for
    /// another, and a newly promoted size owner cannot replay an old copy.
    clipboard_seq: Arc<AtomicU64>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    reader: Mutex<Option<std::thread::JoinHandle<()>>>,
    sock_dir: PathBuf,
    sock_path: PathBuf,
    last_owner_hb: Mutex<Instant>,
}

impl Osc52Channel {
    /// Arm a read-only observer. `pipe-pane` is exclusive, so this uses the
    /// same cross-process owner lease as a VT grid and only runs when the grid
    /// transport is disabled for the displayed terminal pane.
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

    /// Start a new consumer at the current event sequence. This intentionally
    /// skips a value emitted before the consumer began observing, mirroring the
    /// old per-WebSocket watch receiver's `borrow_and_update` baseline.
    pub(crate) fn clipboard_sequence(&self) -> u64 {
        self.clipboard_seq.load(Ordering::Acquire)
    }

    /// Return the latest clipboard write after `seen`, advancing only this
    /// consumer's cursor. Unlike a destructive slot read, every WebSocket can
    /// mark an event seen while only its size owner forwards it.
    pub(crate) fn clipboard_after(&self, seen: &mut u64) -> Option<String> {
        osc52_clipboard_after(&self.clipboard, &self.clipboard_seq, seen)
    }

    /// Keep the exclusive pipe owner lease alive while the terminal snapshot
    /// worker still observes this pane.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Display columns a row occupies once its escape sequences are removed.
    /// Measured as width, not `chars().count()`, so a wide glyph is counted as
    /// the two columns it actually paints.
    fn visible_width(row: &str) -> usize {
        use unicode_width::UnicodeWidthStr;
        UnicodeWidthStr::width(crate::tmux::utils::strip_ansi(row).as_str())
    }

    #[test]
    fn capture_rows_padded_fills_every_row_to_the_pane_width() {
        // The compositor concatenates rows to splice panes side by side, so a
        // short row must be padded or the next pane slides left into the gap.
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
        // Same hazard `lf_to_crlf` fixes for the live seed: `capture-pane`
        // joins rows with a bare LF, which would staircase each pane row off
        // the previous one's end column.
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
        // A row ending in a background fill must not bleed that colour across
        // the border into the pane beside it.
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
        // A wide glyph's continuation cell holds no contents and, unstyled, no
        // style, so counting one column per occupied cell under-counts the row
        // by one. The padding step then appended a space to a row that already
        // filled its pane, making it `cols + 1` wide and shifting every pane to
        // its right by a column.
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

        // The same glyph with room to spare still pads, to the right total.
        let rows = capture_rows_padded("ab漢".as_bytes(), 7, 1);
        assert_eq!(visible_width(&rows[0]), 7, "{:?}", rows[0]);

        // A wide glyph split by the pane edge cannot push the count past `cols`.
        let rows = capture_rows_padded("abc漢".as_bytes(), 4, 2);
        for (i, r) in rows.iter().enumerate() {
            assert_eq!(visible_width(r), 4, "row {i}: {r:?}");
        }
    }

    #[test]
    fn capture_rows_padded_survives_a_one_row_pane_that_wraps() {
        // `resize-pane -y 1` is a real layout, and vt100 panics on a wrapping
        // one-row grid, so this must come back with a single padded row.
        let rows = capture_rows_padded(b"keep", 3, 1);
        assert_eq!(rows.len(), 1);
        assert_eq!(visible_width(&rows[0]), 3);
    }

    #[test]
    fn capture_rows_padded_truncates_content_wider_than_the_pane() {
        // Content wider than the pane wraps inside the parser rather than
        // overflowing the row and shifting the neighbour.
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
                &parser,
                &app_cursor,
                &grid_gen,
                None,
                b"STALE-SNAPSHOT",
                (80, 24),
                Some((&chunk_seq, &settled_chunk_seq, 0)),
            ),
            VtRefreshResult::Busy,
        );
        assert_eq!(
            swap_seeded_parser(
                &parser,
                &app_cursor,
                &grid_gen,
                None,
                b"STALE-SNAPSHOT",
                (80, 24),
                Some((&chunk_seq, &settled_chunk_seq, 1)),
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
                &parser,
                &app_cursor,
                &grid_gen,
                (80, 24),
                &deadline,
                None,
            ),
            VtRefreshResult::Failed,
        );
        assert!(refresh_commits_geometry(VtRefreshResult::Refreshed));
        assert!(!refresh_commits_geometry(VtRefreshResult::Busy));
        assert!(!refresh_commits_geometry(VtRefreshResult::Failed));
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
    fn parse_seed_state_reads_extended_probe_fields() {
        // The probe line carries the drift-detector fields (history_size,
        // pane_height, pane_width) after the mode/cursor fields; all must
        // parse.
        let s = parse_seed_state("1 0 1 0 7 12 0 1 345 48 120");
        assert!(s.alt && !s.mouse && s.mouse_sgr && !s.mouse_all);
        assert_eq!((s.cursor_x, s.cursor_y), (7, 12));
        assert!(!s.cursor_visible && s.app_cursor);
        assert_eq!(
            (s.history_size, s.pane_height, s.pane_width),
            (345, 48, 120)
        );
        // A truncated line falls back to the old defaults instead of erroring,
        // so a probe against an odd tmux build still seeds something usable.
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
        // The probe rides the same tmux invocation as the capture and lands as
        // the LAST output line. The body must survive byte-for-byte, blank
        // padded rows included, with its own trailing newline intact (that is
        // what `strip_trailing_row_terminator` expects to drop).
        let raw = b"row-a\n\n\nrow-d\n0 0 0 0 5 3 1 0 12 24\n";
        let (body, probe) = split_seed_capture(raw);
        assert_eq!(body, b"row-a\n\n\nrow-d\n");
        let post = parse_seed_state(probe);
        assert_eq!((post.cursor_x, post.cursor_y), (5, 3));
        assert_eq!((post.history_size, post.pane_height), (12, 24));

        // No capture rows at all (a zero-height oddity): the single line is
        // the probe, the body is empty.
        let (body, probe) = split_seed_capture(b"0 0 0 0 1 2 1 0 0 5\n");
        assert!(body.is_empty());
        assert_eq!(parse_seed_state(probe).cursor_y, 2);

        assert_eq!(split_seed_capture(b""), (&b""[..], ""));
    }

    #[test]
    fn is_probe_line_rejects_swallowed_capture_rows() {
        // A chained `capture-pane ; display-message` exits 0 even when the
        // display-message half silently fails (pane died mid-chain, verified
        // on tmux 3.6), so the split can hand back a capture row where the
        // probe belongs. The gate must reject anything that isn't the probe's
        // exact all-numeric field shape.
        let fields = SEED_STATE_FMT.split_whitespace().count();
        let probe = vec!["7"; fields].join(" ");
        assert!(is_probe_line(&probe));
        // Shell-ish pane content.
        assert!(!is_probe_line("$ cargo build --release"));
        assert!(!is_probe_line("zsh: command not found: python"));
        // Numeric but truncated (an old tmux missing a format variable, or a
        // half-written line).
        assert!(!is_probe_line(&vec!["1"; fields - 1].join(" ")));
        // One extra field is just as wrong as one missing.
        assert!(!is_probe_line(&vec!["1"; fields + 1].join(" ")));
        assert!(!is_probe_line(""));
    }

    #[test]
    fn seed_probe_agreement_detects_drift() {
        // `capture_seed_snapshot` accepts a snapshot only when the probes
        // bracketing the capture compare equal; every drift a mid-seed pane can
        // exhibit must break equality so the seed retries instead of pairing a
        // stale cursor with newer cells.
        let base = parse_seed_state("0 0 0 0 10 20 1 0 100 40 80");
        assert_eq!(base, parse_seed_state("0 0 0 0 10 20 1 0 100 40 80"));
        // Cursor moved (an echo, a CUP).
        assert_ne!(base, parse_seed_state("0 0 0 0 11 20 1 0 100 40 80"));
        // Scrolled with the cursor pinned to the same row: only history grew.
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 1 0 101 40 80"));
        // Alt-screen flip (a full-screen app starting or quitting).
        assert_ne!(base, parse_seed_state("1 0 0 0 10 20 1 0 100 40 80"));
        // Resize mid-seed changes the cursor's coordinate space.
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 1 0 100 41 80"));
        // Width-only resize rewraps the body while height, history, and cursor
        // can all compare equal.
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 1 0 100 40 79"));
        // DECTCEM toggle (app showed/hid the caret between the probes).
        assert_ne!(base, parse_seed_state("0 0 0 0 10 20 0 0 100 40 80"));
    }

    /// A hand-built channel (no tmux, no forwarder) for registry / sample
    /// tests. `alive` starts false; flip it via the returned handle.
    fn dummy_channel(name: &str, dir: &std::path::Path) -> (Arc<VtChannel>, Arc<AtomicBool>) {
        let alive = Arc::new(AtomicBool::new(false));
        let ch = Arc::new(VtChannel {
            name: name.to_string(),
            owner_id: new_pipe_owner_id(),
            target: format!("{name}:^.0"),
            parser: Arc::new(Mutex::new(vt100::Parser::new(4, 20, SCROLLBACK_LINES))),
            stream: Arc::new(Mutex::new(None)),
            app_cursor: Arc::new(AtomicBool::new(false)),
            alive: alive.clone(),
            wakeup: Arc::new(Mutex::new(None)),
            clipboard: Arc::new(Mutex::new(None)),
            chunk_seq: Arc::new(AtomicU64::new(0)),
            settled_chunk_seq: Arc::new(AtomicU64::new(0)),
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
        });
        (ch, alive)
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
        // The live composite asks for pane 0's rectangle as tmux reports it,
        // which can differ from the grid's own size mid-resize. Every returned
        // row must occupy exactly the requested width, and there must be
        // exactly the requested number of them, or the panes spliced to the
        // right of this one shift.
        let name = format!("aoe_test_vt_padded_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel(&name, dir.path());
        // Grid is 4 rows x 20 cols (see `dummy_channel`).
        ch.parser
            .lock()
            .unwrap()
            .process(b"hello\r\nworld\r\n\x1b[41mfilled");

        // Exact rectangle.
        let (rows, cursor) = ch.sample_rows_padded(20, 4).expect("sample");
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
        // Cursor comes straight off the grid and is always trustworthy.
        assert!(cursor.position_reliable);

        // Narrower and shorter than the grid: truncate, never overflow.
        let (rows, _) = ch.sample_rows_padded(6, 2).expect("sample");
        assert_eq!(rows.len(), 2);
        for r in &rows {
            assert_eq!(crate::tmux::utils::strip_ansi(r).chars().count(), 6);
        }

        // Taller than the grid (tmux says the pane grew before the grid caught
        // up): the extra rows are blank filler at the right width, not rows
        // borrowed from elsewhere.
        let (rows, _) = ch.sample_rows_padded(10, 6).expect("sample");
        assert_eq!(rows.len(), 6);
        for (i, r) in rows.iter().enumerate() {
            let plain = crate::tmux::utils::strip_ansi(r);
            assert_eq!(plain.chars().count(), 10, "row {i}: {r:?}");
            if i >= 4 {
                assert!(plain.trim().is_empty(), "row {i} should be filler: {r:?}");
            }
        }
    }

    #[test]
    fn acquire_does_not_reuse_a_dead_channel() {
        // Regression for the session-restart corpse: kill_clean recreates the
        // tmux session under the same name, the old channel dies, but a
        // surviving viewer's Arc keeps it registered. `acquire` must refuse
        // the dead entry (pre-fix it returned it, stranding every new viewer
        // on the capture fallback and re-pinning the corpse in the registry).
        let name = format!("aoe_test_vt_dead_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (dead, _alive) = dummy_channel(&name, dir.path());
        REGISTRY
            .lock()
            .unwrap()
            .insert(name.clone(), Arc::downgrade(&dead));

        // With no real pane to arm against, a correct `acquire` reports None
        // rather than handing back the corpse.
        let got = VtChannel::acquire(&name);
        assert!(
            got.is_none_or(|c| c.is_alive()),
            "acquire must never return a dead channel"
        );

        REGISTRY.lock().unwrap().remove(&name);
    }

    #[test]
    fn concurrent_acquire_for_one_session_serializes_without_deadlock() {
        // Two racing acquires for the same (nonexistent) session must both
        // come back (None here, since there is no pane to arm), not deadlock
        // on the per-session arm lock, and a dead registry entry must not
        // wedge the serialized path either.
        let name = format!("aoe_test_vt_race_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (dead, _alive) = dummy_channel(&name, dir.path());
        REGISTRY
            .lock()
            .unwrap()
            .insert(name.clone(), Arc::downgrade(&dead));

        let n1 = name.clone();
        let t1 = std::thread::spawn(move || VtChannel::acquire(&n1));
        let n2 = name.clone();
        let t2 = std::thread::spawn(move || VtChannel::acquire(&n2));
        let r1 = t1.join().expect("thread 1");
        let r2 = t2.join().expect("thread 2");
        assert!(
            r1.is_none_or(|c| c.is_alive()) && r2.is_none_or(|c| c.is_alive()),
            "neither racer may receive a dead channel"
        );
        // Finished arm locks are pruned by the next acquire (the last
        // finisher retains only its own): after an unrelated acquire runs,
        // the raced session's lock must be gone from the map.
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
        // The cache must key on the grid generation: same gen => cached
        // assembly (even if the parser has quietly advanced, the reader
        // always bumps gen first in real operation); bumped gen => fresh
        // assembly. `reconcile_grid` stays quiescent here because
        // `last_size_check` is fresh, so no tmux fork runs.
        let name = format!("aoe_test_vt_cache_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel(&name, dir.path());

        ch.parser.lock().unwrap().process(b"one");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let (first, _) = ch.sample(4);
        assert!(first.contains("one"), "fresh assembly:\n{first:?}");

        // Advance the parser WITHOUT bumping gen: the cache must still serve
        // the old frame (this is what makes an idle pane's cadence cheap).
        ch.parser.lock().unwrap().process(b" two");
        let (cached, _) = ch.sample(4);
        assert!(
            !cached.contains("two"),
            "same generation must serve the cached assembly:\n{cached:?}"
        );

        // Bump gen (what the reader does per chunk): fresh assembly.
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let (fresh, _) = ch.sample(4);
        assert!(
            fresh.contains("two"),
            "bumped generation must reassemble:\n{fresh:?}"
        );

        // A different window size also misses the cache.
        let (wider, _) = ch.sample(3);
        assert!(wider.contains("two"), "window change must reassemble");
    }

    #[test]
    fn seed_replays_application_cursor_mode() {
        // `#{keypad_cursor_flag}` reports DECCKM; the seed must replay it so a
        // channel armed while an app is already in application-cursor mode
        // (vim, a full-screen agent) encodes arrows as `ESC O A` from the
        // first keystroke, instead of misencoding until the app re-emits the
        // mode. This also pins the vt100 primitive: `ESC [ ? 1 h` must
        // surface as `application_cursor()`, which is what `seed_parser`
        // stores into the channel's input-path flag.
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

        // The sample cache keys on the grid generation; every parsed chunk
        // must bump it, or a stale cached assembly would be served after new
        // output landed.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let ctx = ReaderCtx {
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0))),
            stop: stop.clone(),
            seeded: Arc::new(AtomicBool::new(true)),
            stream: Arc::new(Mutex::new(None)),
            app_cursor: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Mutex::new(None)),
            clipboard: Arc::new(Mutex::new(None)),
            chunk_seq: Arc::new(AtomicU64::new(0)),
            settled_chunk_seq: Arc::new(AtomicU64::new(0)),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            grid_gen: grid_gen.clone(),
            signals: Arc::new(ViewerSignals::new()),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
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
    fn seed_swap_abandons_a_chunk_that_landed_during_capture() {
        use std::io::Write;

        // #3617: `capture_seed_stream` forks tmux, so a chunk can reach the
        // live parser between the snapshot and the swap. Replacing the parser
        // would drop it from both grids and pipe-pane cannot redeliver it, so
        // the swap must stand down instead. Deterministic without tmux: drive
        // `run_reader` over a raw socket, then call the swap directly with the
        // generation a reseed would have sampled before its capture.
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
            seeded: Arc::new(AtomicBool::new(true)),
            stream: Arc::new(Mutex::new(None)),
            app_cursor: app_cursor.clone(),
            alive: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Mutex::new(None)),
            clipboard: Arc::new(Mutex::new(None)),
            chunk_seq: chunk_seq.clone(),
            settled_chunk_seq: settled_chunk_seq.clone(),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            grid_gen: grid_gen.clone(),
            signals: Arc::new(ViewerSignals::new()),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        // The generation a reseed reads before forking its capture.
        let since = grid_gen.load(Ordering::Relaxed);
        // The pane resumes output while that capture is in flight.
        conn.write_all(b"post-snapshot-chunk").expect("write");
        let deadline = Instant::now() + Duration::from_secs(5);
        while grid_gen.load(Ordering::Relaxed) == since {
            assert!(Instant::now() < deadline, "reader never applied the chunk");
            std::thread::sleep(Duration::from_millis(2));
        }

        let seed = assemble_seed_stream(b"snapshot-body\n", &PaneSeedState::default(), 24);
        assert_eq!(
            swap_seeded_parser(
                &parser,
                &app_cursor,
                &grid_gen,
                Some(since),
                &seed,
                (80, 24),
                Some((&chunk_seq, &settled_chunk_seq, 0)),
            ),
            VtRefreshResult::Busy,
            "swap must stand down once a chunk has landed"
        );
        let grid = parser.lock().expect("parser").screen().contents();
        assert!(
            grid.contains("post-snapshot-chunk"),
            "the raced chunk must survive in the live grid:\n{grid:?}"
        );

        // Same swap once the grid is quiet at the sampled generation: applies.
        let quiet = grid_gen.load(Ordering::Relaxed);
        let expected_chunk_seq = chunk_seq.load(Ordering::Acquire);
        assert_eq!(
            swap_seeded_parser(
                &parser,
                &app_cursor,
                &grid_gen,
                Some(quiet),
                &seed,
                (80, 24),
                Some((&chunk_seq, &settled_chunk_seq, expected_chunk_seq,)),
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
            settled_chunk_seq: Arc::new(AtomicU64::new(0)),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            grid_gen: Arc::new(AtomicU64::new(0)),
            signals: Arc::new(ViewerSignals::new()),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        let pair: ChangeWakeup = Arc::new((Mutex::new(0), Condvar::new()));
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
            settled_chunk_seq: Arc::new(AtomicU64::new(0)),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            grid_gen: Arc::new(AtomicU64::new(0)),
            signals: Arc::new(ViewerSignals::new()),
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
    fn osc52_observer_publishes_copy_without_a_vt_grid() {
        use std::io::Write;

        // Terminal fallback renders capture-pane cells, not a vt100 grid. Its
        // raw observer must still extract a copy from pipe-pane's byte stream.
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
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
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
            chunk_seq,
            settled_chunk_seq: settled_chunk_seq.clone(),
            last_chunk_ms: last_chunk_ms.clone(),
            prev_gap_ms: prev_gap_ms.clone(),
            grid_gen: Arc::new(AtomicU64::new(0)),
            signals: Arc::new(ViewerSignals::new()),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        // Wait for the reader to publish the nth chunk's complete parser
        // and timing state. Writing the next chunk only after the previous is
        // settled also keeps them as separate reads (a unix stream is a byte
        // stream, so two pending writes could otherwise coalesce).
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

    #[test]
    fn reader_fences_seed_windows_and_still_taps_clipboard() {
        use std::io::Write;

        // Output received before the initial capture is not replayed because
        // that later snapshot already contains it. The read still advances the
        // seed fence, and OSC 52 remains observable while the grid is unseeded.
        // Once seeded, every read advances the same fence before waiting on the
        // parser so an authoritative refresh cannot duplicate a queued chunk.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let seeded = Arc::new(AtomicBool::new(false));
        let parser = Arc::new(Mutex::new(vt100::Parser::new(6, 40, 0)));
        let clipboard: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let chunk_seq = Arc::new(AtomicU64::new(0));
        let settled_chunk_seq = Arc::new(AtomicU64::new(0));
        let ctx = ReaderCtx {
            parser: parser.clone(),
            stop: stop.clone(),
            seeded: seeded.clone(),
            stream: Arc::new(Mutex::new(None)),
            app_cursor: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Mutex::new(None)),
            clipboard: clipboard.clone(),
            chunk_seq: chunk_seq.clone(),
            settled_chunk_seq: settled_chunk_seq.clone(),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            grid_gen: grid_gen.clone(),
            signals: Arc::new(ViewerSignals::new()),
        };
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        // Pre-seed: pane output plus an OSC 52 copy, all while `seeded == false`.
        conn.write_all(b"PRE-SEED-OUTPUT\x1b]52;c;aGVsbG8=\x07")
            .expect("write pre-seed");
        let deadline = Instant::now() + Duration::from_secs(5);
        while clipboard.lock().unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "reader never tapped the pre-seed OSC 52"
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

        // Hold the parser while a live chunk arrives. The sequence must move
        // before the reader can acquire this lock, otherwise a concurrent seed
        // could install a snapshot containing the chunk and then apply it again.
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
        drop(parser_guard);
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
    fn reconcile_step_resizes_and_confirms_drift_before_reseeding() {
        // (tmux, grid, pending, grid_gen) -> decision
        let cases = [
            // Geometry and cursor agree: nothing to do, any armed drift clears.
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
            // Geometry wins over a cursor mismatch: the reflow moves it anyway.
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
            // First sighting of a cursor mismatch only arms; a probe that raced
            // the byte stream must not cost a reseed.
            (
                (80, 24, 5, 3),
                (80, 24, 4, 3),
                None,
                7,
                GridReconcile::ArmDrift,
            ),
            // Still mismatched but the grid took output in between, so the
            // earlier probe was a race, not drift. Re-arm at the new generation.
            (
                (80, 24, 5, 3),
                (80, 24, 4, 3),
                Some(7),
                8,
                GridReconcile::ArmDrift,
            ),
            // Same mismatch, generation unchanged: no output could explain it.
            (
                (80, 24, 5, 3),
                (80, 24, 4, 3),
                Some(7),
                7,
                GridReconcile::Reseed,
            ),
            // Row drift alone is enough (the doubled-output case shifts rows,
            // not columns).
            (
                (80, 24, 5, 6),
                (80, 24, 5, 3),
                Some(0),
                0,
                GridReconcile::Reseed,
            ),
            // A pane parked at a pending wrap: tmux reports `cursor_x ==
            // pane_width` (verified against tmux 3.6) while the seeded grid is
            // clamped to `pane_width - 1` by vt100's CUP. Reading that as drift
            // reseeds every other pass forever, since the reseed reproduces the
            // same clamped column.
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
            // The clamp is per-pane-width, not a blanket "ignore column 9".
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
        // A partial parse would hand `reconcile_step` a bogus cursor, which reads
        // as drift and reseeds the grid every pass. Short and unparseable lines
        // must come back None so the reconcile pass simply skips.
        let cases = [
            ("80 24 5 3", Some((80u16, 24u16, 5u16, 3u16))),
            // tmux pads with a trailing newline.
            ("80 24 5 3\n", Some((80, 24, 5, 3))),
            // Extra trailing fields are ignored, not an error.
            ("80 24 5 3 99", Some((80, 24, 5, 3))),
            // A pane that vanished mid-probe: fewer fields than asked for.
            ("80 24 5", None),
            ("80 24", None),
            ("", None),
            // A format tmux could not resolve comes back non-numeric.
            ("80 24 5 #{cursor_y}", None),
            // Negative / overflowing values are not u16.
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
        assert_eq!(sc.feed(b"plain text \x1b[31m"), None);
        // Split at every byte boundary of the opener.
        let opener = b"\x1b[?2026h";
        for (i, _) in opener.iter().enumerate().skip(1) {
            let mut split = SyncOutputScanner::new();
            assert_eq!(split.feed(&opener[..i]), None);
            assert_eq!(split.feed(&opener[i..]), Some(true), "split at {i}");
        }
        assert_eq!(sc.feed(b"\x1b[?2026h"), Some(true));
        // 2026 inside a parameter list, closing.
        assert_eq!(sc.feed(b"\x1b[?25;2026l"), Some(false));
        // Other private modes are not the bracket.
        assert_eq!(sc.feed(b"\x1b[?1049h\x1b[?25l"), None);
        // A non-private CSI with 2026 is not the bracket either.
        assert_eq!(sc.feed(b"\x1b[2026h"), None);
        // Last transition in a chunk wins.
        assert_eq!(sc.feed(b"\x1b[?2026h frame \x1b[?2026l"), Some(false));
    }

    #[test]
    fn viewer_signals_hold_opens_and_closes() {
        let signals = ViewerSignals::new();
        assert!(!signals.hold_active());
        signals.begin_hold();
        assert!(signals.hold_active());
        // Re-opening does not restart the clock.
        let since = signals.sync_hold_since_ms.load(Ordering::Relaxed);
        signals.begin_hold();
        assert_eq!(signals.sync_hold_since_ms.load(Ordering::Relaxed), since);
        signals.end_hold();
        assert!(!signals.hold_active());
        assert!(!signals.frame_incomplete());

        // A repaint slower than the wakeup hold stops suppressing publication
        // but must still read as incomplete, or the sampler would serve the
        // half-drawn grid instead of the last whole frame it already has.
        signals.begin_hold();
        let since = signals.sync_hold_since_ms.load(Ordering::Relaxed);
        for (elapsed, hold, incomplete) in [
            (0, true, true),
            (SYNC_HOLD_MAX_MS - 1, true, true),
            (SYNC_HOLD_MAX_MS, false, true),
            (SYNC_BRACKET_ABANDON_MS - 1, false, true),
            // Past this the app is stuck and its partial screen is all there is.
            (SYNC_BRACKET_ABANDON_MS, false, false),
        ] {
            assert_eq!(
                signals.open_within(since + elapsed, SYNC_HOLD_MAX_MS),
                hold,
                "hold at {elapsed}ms"
            );
            assert_eq!(
                signals.open_within(since + elapsed, SYNC_BRACKET_ABANDON_MS),
                incomplete,
                "incomplete at {elapsed}ms"
            );
        }
    }

    #[test]
    fn reader_holds_viewer_wakeups_inside_a_synchronized_output_bracket() {
        use std::io::Write;

        // A full-screen agent brackets each repaint in DEC 2026. The grid
        // keeps parsing (generation bumps) but viewers must not wake until
        // the bracket closes, or they would sample a half-drawn frame.
        let dir = tempfile::tempdir().expect("tempdir");
        let sock = dir.path().join("s.sock");
        let listener = UnixListener::bind(&sock).expect("bind");
        let stop = Arc::new(AtomicBool::new(false));
        let grid_gen = Arc::new(AtomicU64::new(0));
        let signals = Arc::new(ViewerSignals::new());
        let wakeup: ChangeWakeup = Arc::new((Mutex::new(0u64), Condvar::new()));
        let ctx = ReaderCtx {
            parser: Arc::new(Mutex::new(vt100::Parser::new(24, 80, 0))),
            stop: stop.clone(),
            seeded: Arc::new(AtomicBool::new(true)),
            stream: Arc::new(Mutex::new(None)),
            app_cursor: Arc::new(AtomicBool::new(false)),
            alive: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Mutex::new(Some(wakeup.clone()))),
            clipboard: Arc::new(Mutex::new(None)),
            chunk_seq: Arc::new(AtomicU64::new(0)),
            settled_chunk_seq: Arc::new(AtomicU64::new(0)),
            last_chunk_ms: Arc::new(AtomicU64::new(0)),
            prev_gap_ms: Arc::new(AtomicU64::new(u64::MAX)),
            grid_gen: grid_gen.clone(),
            signals: signals.clone(),
        };
        let rx = signals.changed_tx.subscribe();
        let reader = std::thread::spawn(move || run_reader(listener, ctx));
        let mut conn = UnixStream::connect(&sock).expect("connect");

        conn.write_all(b"\x1b[?2026h\x1b[2J\x1b[HPART-A")
            .expect("write");
        let deadline = Instant::now() + Duration::from_secs(5);
        while grid_gen.load(Ordering::Relaxed) < 1 {
            assert!(Instant::now() < deadline, "reader never parsed the chunk");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(signals.hold_active(), "bracket opened: hold must be active");
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
        while grid_gen.load(Ordering::Relaxed) < 2 {
            assert!(Instant::now() < deadline, "reader never parsed the close");
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(!signals.hold_active(), "bracket closed: hold released");
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
    fn sample_serves_last_complete_frame_while_bracket_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (ch, _alive) = dummy_channel("aoe-vt-hold-test", dir.path());
        ch.parser.lock().unwrap().process(b"before");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let deadline = crate::tmux::TmuxCommandDeadline::new();
        let (first, _) = ch.sample_with_deadline(4, &deadline);
        assert!(first.contains("before"));

        // Output lands inside a bracket: the sample must not follow it yet.
        ch.signals.begin_hold();
        ch.parser.lock().unwrap().process(b"\r\x1b[Kafter");
        ch.grid_gen.fetch_add(1, Ordering::Relaxed);
        let (held, _) = ch.sample_with_deadline(4, &deadline);
        assert_eq!(
            held, first,
            "mid-bracket sample serves the last complete frame"
        );

        ch.signals.end_hold();
        let (fresh, _) = ch.sample_with_deadline(4, &deadline);
        assert!(
            fresh.contains("after"),
            "closing the bracket publishes the new frame"
        );
        assert!(!fresh.contains("before"));
    }
}
