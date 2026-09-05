//! Live terminal view for the web dashboard.
//!
//! The agent surface renders from the shared VT channel (`crate::tmux::vt`)
//! when one can be armed (`[tmux] vt_live`, tmux >= 3.4, unix): the pane's
//! bytes stream through `pipe-pane` into an in-process grid, frames publish
//! the moment the grid changes (held while the app is inside a DEC 2026
//! synchronized-output bracket, so a half-drawn repaint is never shipped),
//! and keystrokes go back over the same socket. The paired host and
//! container shells, and every fallback, poll `tmux capture-pane` snapshots
//! on a cadence and deliver input with `tmux send-keys -H`. Either way there
//! is no PTY and no `tmux attach`: scrollback is just a bigger window the
//! client renders and scrolls natively, and the agent keeps running while
//! the user reads.
//!
//! Protocol (one WS per viewer, route `/sessions/{id}/live-ws`):
//!
//! Server -> client, JSON text frames:
//!   `{"type":"frame","content":"<ANSI text>","rows":..,"history":..,
//!     "cursor":{"x":..,"y":..}|null,
//!     "altScreen":bool,"mouse":bool,"mouseSgr":bool}`
//!   `content` is verbatim `capture-pane -e` output for the requested
//!   window: history lines first, the live screen as the last `rows`
//!   lines (trailing blank screen rows preserved). `altScreen` /`mouse` /
//!   `mouseSgr` mirror tmux's `#{alternate_on}` / `#{mouse_any_flag}` /
//!   `#{mouse_sgr_flag}`: when the pane is a full-screen mouse app the
//!   client forwards the wheel to it (as input bytes) instead of widening
//!   the capture window, since the alternate screen has no scrollback.
//!   A composited frame also carries `"pane0"` as
//!   `{"cols":..,"rows":..,"left":..,"top":..}`. Cursor coordinates are
//!   translated onto the window grid before emission; clients subtract the
//!   same origin when forwarding pointer cells. Single-pane frames serialize
//!   `pane0` as `null`. The origin fields are optional for older clients and
//!   default to zero for frames from older servers. Every frame carries a
//!   monotonic `seq`.
//!   `{"type":"patch","seq":..,"base":..,"shift":k,"lines":[[i,"<ANSI>"],..],
//!     ...same geometry/cursor/flag fields as a frame}`: sent instead of a
//!   frame when the client advertised `caps.patch` and few rows changed. The
//!   client drops the first `shift` rows of its previous window (history
//!   grew by that many lines), appends `shift` blank rows, then replaces the
//!   listed rows. `base` names the `seq` the patch applies to; a client that
//!   is not at `base` sends `{"type":"resync"}` and receives a full frame.
//!   `{"type":"size_owner","is_owner":bool}`: whether this client holds
//!     the session's size-owner lock. Only the owner resizes the shared
//!     tmux window and may type; a non-owner renders best-effort at the
//!     owner's grid and shows a "take over" affordance. A visible
//!     non-owner at fast cadence auto-reclaims the lock (claim, never
//!     steal) once the holder releases it, so ownership returns without
//!     another "take over" tap.
//!   `{"type":"transport","grid":bool}`: which transport is producing frames,
//!     sent on the first frame and whenever it flips. `false` means the
//!     capture fallback, which cannot suppress a half-drawn repaint.
//!   `{"type":"clipboard","text":"..."}`: an OSC 52 clipboard write emitted
//!     by the pane. The browser resolves it against the user gesture that
//!     triggered the agent's copy action.
//!
//! Client -> server:
//!   Binary frames: raw bytes for the pane (keystrokes, escape
//!     sequences, bracketed paste). Dropped in read-only mode and for a
//!     non-owner client.
//!   `{"type":"resize","cols":..,"rows":..}`: claim the size-owner lock
//!     and, if won, resize the (detached) tmux window to the client's
//!     grid. The lock lives in tmux user options so the web desktop view
//!     and the native TUI honor the same owner; it is released (and
//!     `window-size latest` restored) when the owner disconnects.
//!   `{"type":"claim"}`: explicit take-over from a non-owner; steals the
//!     lock even from a live holder and sizes the window to this client.
//!   `{"type":"window","lines":N}`: total capture window (history +
//!     screen). Clamped to [screen rows, MAX_WINDOW_LINES].
//!   `{"type":"cadence","fast":bool}`: capture cadence. Fast while the
//!     client is at the live edge and visible; idle while reading
//!     scrollback or backgrounded. Like the TUI's live mode, the loop
//!     keeps capturing while the user reads (the agent runs on); a
//!     scrolled-up client just asks for a bigger window and renders it
//!     against a stable position via its spacer model.
//!   `{"type":"resync"}`: the client lost patch continuity; the next publish
//!     is a full frame.
//!   `{"type":"caps","deflate":bool,"patch":bool}`: client capability
//!     advertisement. `patch:true` enables row patches (above).
//!     With `deflate:true`, frame messages switch from JSON text to
//!     BINARY: a connection-lifetime raw-deflate stream, sync-flushed per
//!     frame, carrying `u32-LE length || frame JSON` records in the
//!     plaintext. One stream (not per-message compression) on purpose:
//!     consecutive frames are near-identical, so the shared dictionary
//!     turns each into back-references, a delta encoding without diff
//!     heuristics. Clients without `DecompressionStream` (and stale PWA
//!     bundles, which never send caps) keep receiving text frames;
//!     `size_owner` and close frames stay text/control always. Old
//!     servers ignore the unknown message type harmlessly.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tracing::{debug, warn};

use super::pane::{
    close_early, wait_for_tmux_ready, PaneReadiness, CLOSE_CODE_GOING_AWAY, CLOSE_CODE_PTY_DEAD,
    CLOSE_CODE_TRY_AGAIN_LATER,
};
use super::AppState;
use crate::tmux::{SIZE_OWNER_HEARTBEAT, SIZE_OWNER_TTL};

/// Capture cadence while the client is at the live edge. Matches the
/// TUI's live-send fast interval: tight enough that typed echo feels
/// attach-like, while the content dedup keeps idle panes free.
const CAPTURE_INTERVAL_FAST_MS: u64 = 50;
/// Cadence while the client reads scrollback or is backgrounded. The
/// scrolled-up window can be thousands of lines, so frames are big;
/// at this rate a streaming agent costs at most a few frames per second.
const CAPTURE_INTERVAL_IDLE_MS: u64 = 250;
/// Minimum gap between snapshot samples. This caps a spewing pane at roughly
/// 60fps instead of continuously forking capture-pane.
const FRAME_MIN_INTERVAL_MS: u64 = 16;
/// Wait ceiling while a VT channel drives the loop. Output wakes the loop
/// itself, so the timer only serves the size-owner heartbeat and death checks.
const GRID_CEILING_MS: u64 = 250;
/// After the owner resizes the window, frames whose pane geometry still
/// disagrees with the requested grid are withheld for this long, so the client
/// sees one clean repaint instead of a clear, a half-draw, and a settle.
const RESIZE_SETTLE_MS: u64 = 300;
/// A freshly armed channel seeded from `capture-pane`, which cannot tell
/// whether the app was mid-repaint. Its first publish waits for output to
/// arrive and go quiet for this long (a torn seed is completed by the rest of
/// the repaint; a whole one is confirmed by the next bracket closing).
const FIRST_PUBLISH_QUIET_MS: u64 = 30;
/// Upper bound on that first-publish wait, so an idle pane still paints.
const FIRST_PUBLISH_MAX_WAIT_MS: u64 = 150;
/// A channel older than this was seeded long before this viewer connected;
/// its grid has been reconciled by live output and needs no opening hold.
const FRESH_SEED_MAX_AGE: Duration = Duration::from_secs(1);
/// How often the grid path re-checks the window's pane count. A split window
/// is composited from `capture-pane`, which the single-pane grid cannot do.
const PANE_COUNT_PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Row patches beyond this fraction of the window are sent as full frames.
const PATCH_MAX_CHANGED_RATIO: f32 = 0.5;
/// Upper bound on the capture window. tmux history defaults to 2000
/// lines per pane; this leaves headroom for raised limits without
/// letting a client demand unbounded captures.
const MAX_WINDOW_LINES: usize = 4000;
/// Floor for the capture window when the client hasn't sized yet.
const DEFAULT_WINDOW_LINES: usize = 50;
/// Keepalive ping interval; the recv side relies on the browser's pong.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Floor between drift re-asserts (see the capture loop): both known
/// writers dedup, so this only matters against an unknown one.
const REASSERT_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// After a drift target proves unreachable (same geometry didn't move after
/// the last re-assert), wait this long before retrying it once, so a transient
/// tmux failure still recovers without spinning the 2s repaint loop.
const STUCK_REASSERT_RETRY: Duration = Duration::from_secs(30);

/// The owner loop's view of a size drift: the grid the client wants versus the
/// pane tmux currently yields. Two identical tuples across re-asserts mean the
/// last resize changed nothing, i.e. the target is unreachable.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DriftGeometry {
    want_cols: u16,
    want_rows: u16,
    pane_cols: u16,
    pane_rows: u16,
}

/// Suppresses re-asserting a drift target that has proven unreachable.
/// Re-asserting an identical resize only repaints the pane (#2766); recovery is
/// preserved because any genuine geometry change is a different tuple and a
/// stuck tuple is retried once after [`STUCK_REASSERT_RETRY`].
struct ReassertGuard {
    last: Option<(DriftGeometry, Instant)>,
    retry_after: Duration,
}

impl ReassertGuard {
    fn new(retry_after: Duration) -> Self {
        Self {
            last: None,
            retry_after,
        }
    }

    /// True when this drift geometry should trigger a re-assert. Suppresses an
    /// identical geometry seen within `retry_after` of the last re-assert (the
    /// previous resize changed nothing, so repeating it can't help); allows a
    /// changed geometry immediately and an unchanged one again after the retry
    /// window elapses.
    fn should_reassert(&mut self, geom: DriftGeometry, now: Instant) -> bool {
        match self.last {
            Some((last, at)) if last == geom && now.duration_since(at) < self.retry_after => false,
            _ => {
                self.last = Some((geom, now));
                true
            }
        }
    }

    /// Forget the last target so the next drift re-asserts immediately. Called
    /// when the pane reaches the requested grid.
    fn reset(&mut self) {
        self.last = None;
    }
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum LiveControlMessage {
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
    #[serde(rename = "window")]
    Window { lines: usize },
    #[serde(rename = "cadence")]
    Cadence { fast: bool },
    /// Request the lock when it is vacant, without resizing or displacing a
    /// live owner. Mobile startup uses this while the soft keyboard prevents a
    /// safe grid measurement.
    #[serde(rename = "claim_if_vacant")]
    ClaimIfVacant,
    /// Explicit "take over" from a non-owner client: steal the size-owner
    /// lock even from a live holder (a user tap is intentional, unlike the
    /// passive flap the heartbeat guards against).
    #[serde(rename = "claim")]
    Claim,
    /// Capability advertisement; see the module doc. `deflate:true` switches
    /// frame delivery to the compressed binary stream; `patch:true` enables
    /// row patches.
    #[serde(rename = "caps")]
    Caps {
        #[serde(default)]
        deflate: bool,
        #[serde(default)]
        patch: bool,
    },
    /// The client lost patch continuity and needs a full frame.
    #[serde(rename = "resync")]
    Resync,
}

/// Which transport renders a live surface. The agent pane takes the shared VT
/// grid when one can be armed; the paired shells stay on snapshots, whose
/// seed-free capture avoids the doubled-prompt repaint a shell can show while
/// a grid seeds under it (#3315).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveTransport {
    Grid,
    Snapshot,
}

/// Shared per-connection knobs the recv loop writes and the capture
/// loop reads.
struct LiveSettings {
    window_lines: AtomicUsize,
    fast: AtomicBool,
    /// Grid from the latest client resize. Rows double as the window
    /// floor so a shrunk window can never clip the live screen; both
    /// dimensions feed the drift re-assert below.
    screen_rows: AtomicU64,
    screen_cols: AtomicU64,
    /// True while this connection holds the cross-process size-owner lock.
    /// Only the owner resizes the tmux window and accepts input; the capture
    /// loop flips this false when the lock is lost to another client.
    is_owner: AtomicBool,
    /// Client advertised `caps.deflate`: frames go out as the compressed
    /// binary stream instead of JSON text. Set-once (a client never revokes).
    deflate: AtomicBool,
    /// Client advertised `caps.patch`: publish row patches when few rows
    /// changed. Set-once.
    patch: AtomicBool,
    /// The next publish must be a full frame (client resync).
    force_full: AtomicBool,
    /// [`live_now_ms`] until which frames at a pane geometry other than the
    /// requested grid are withheld after an owner resize; 0 when none.
    resize_settle_until_ms: AtomicU64,
}

static LIVE_CLOCK: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

fn live_now_ms() -> u64 {
    LIVE_CLOCK.elapsed().as_millis() as u64
}

/// Whether a frame must be withheld during the post-resize settle window:
/// the window is open and the pane has not yet reached the requested grid.
fn resize_settle_holds(now_ms: u64, until_ms: u64, want: (u16, u16), have: (u16, u16)) -> bool {
    now_ms < until_ms && want != have
}

/// Rewrite bare cursor-key sequences for an app in DECCKM (application
/// cursor) mode. The browser always emits the normal-mode `CSI A..D/H/F`;
/// `send-keys -H` delivered those verbatim and tmux never translated them,
/// so arrows misfired in vim-like apps. Modified forms (`CSI 1;5A`) are left
/// alone: they carry no mode-dependent encoding.
fn translate_cursor_keys(bytes: &[u8], app_cursor: bool) -> std::borrow::Cow<'_, [u8]> {
    if !app_cursor || !bytes.contains(&0x1b) {
        return std::borrow::Cow::Borrowed(bytes);
    }
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b
            && bytes.get(i + 1) == Some(&b'[')
            && bytes
                .get(i + 2)
                .is_some_and(|c| matches!(c, b'A'..=b'D' | b'H' | b'F'))
        {
            out.extend_from_slice(&[0x1b, b'O', bytes[i + 2]]);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    std::borrow::Cow::Owned(out)
}

/// Split a frame's content into rows. Both transports terminate every row,
/// including the last, with `\n`, so the trailing empty piece is not a row.
fn frame_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Rows of `next` that differ from `prev` once `prev` is slid up by `shift`
/// rows (history grew by `shift` lines, so row `i` of the new window was row
/// `i + shift` of the old). `None` when a full frame is the better message:
/// the windows differ in height, or more than [`PATCH_MAX_CHANGED_RATIO`] of
/// the rows changed.
fn plan_patch<'a>(
    prev: &[String],
    next: &[&'a str],
    shift: usize,
) -> Option<Vec<(usize, &'a str)>> {
    if prev.len() != next.len() || next.is_empty() {
        return None;
    }
    let n = next.len();
    let shift = shift.min(n);
    let mut changed = Vec::new();
    for (i, row) in next.iter().enumerate() {
        let old = prev.get(i + shift).map(String::as_str).unwrap_or("");
        if *row != old {
            changed.push((i, *row));
        }
    }
    if changed.len() as f32 > n as f32 * PATCH_MAX_CHANGED_RATIO {
        return None;
    }
    Some(changed)
}

/// JSON control frame telling the client whether it currently owns the
/// session's size (and may resize/type) or is a read-only viewer.
fn size_owner_json(is_owner: bool) -> String {
    serde_json::json!({ "type": "size_owner", "is_owner": is_owner }).to_string()
}

fn clipboard_json(text: &str) -> String {
    serde_json::json!({ "type": "clipboard", "text": text }).to_string()
}

/// Which transport is producing frames. The grid can be unavailable for
/// reasons no user can see (an old tmux, a pane that would not seed, a split
/// window), and the fallback tears where the grid does not, so a viewer
/// debugging "it still tears" needs to know which one it has.
fn transport_json(grid: bool) -> String {
    serde_json::json!({ "type": "transport", "grid": grid }).to_string()
}

/// Whether this connection may push the pane's OSC 52 copies into the
/// viewer's browser clipboard. Mirrors the input gate: a `--read-only`
/// viewer never typed or clicked, so an agent copy driven by whoever *is*
/// driving the session must not silently rewrite that viewer's system
/// clipboard (the browser side falls back to an ungestured
/// `writeClipboard` when no selection release armed the write).
#[cfg(unix)]
fn clipboard_forward_enabled(
    mode: crate::session::config::TmuxSettingMode,
    read_only: bool,
) -> bool {
    !read_only && mode != crate::session::config::TmuxSettingMode::Disabled
}

/// Connection-lifetime deflate stream for frame messages (module doc, `caps`).
/// One raw-deflate stream sync-flushed per frame, so every binary WS message
/// is immediately decodable while the compression dictionary carries across
/// frames: consecutive captures share most of their content, so each frame
/// compresses to back-references into the previous ones. That cross-frame
/// reuse is the point; per-message compression can't see it, and it is what
/// keeps scroll bursts (60fps of near-identical screens) to a few hundred
/// bytes each instead of the full window.
struct FrameDeflater {
    stream: flate2::Compress,
    input: Vec<u8>,
}

impl FrameDeflater {
    fn new() -> Self {
        Self {
            // Raw deflate, no zlib wrapper: the browser inflates with
            // `DecompressionStream("deflate-raw")`.
            stream: flate2::Compress::new(flate2::Compression::fast(), false),
            input: Vec::new(),
        }
    }

    /// Compress one frame into one binary WS payload. The plaintext record is
    /// `u32-LE length || json`, so the client re-splits the decompressed byte
    /// stream into frames no matter how the inflater chunks its output.
    /// Returns `None` on a corrupt stream state (not expected in practice);
    /// the caller then degrades to text frames, which every client accepts.
    fn frame(&mut self, json: &str) -> Option<Vec<u8>> {
        self.input.clear();
        self.input
            .extend_from_slice(&(json.len() as u32).to_le_bytes());
        self.input.extend_from_slice(json.as_bytes());
        let mut out = Vec::with_capacity(self.input.len() / 8 + 64);
        let mut consumed = 0usize;
        loop {
            out.reserve(1024);
            let before = self.stream.total_in();
            self.stream
                .compress_vec(
                    &self.input[consumed..],
                    &mut out,
                    flate2::FlushCompress::Sync,
                )
                .ok()?;
            consumed += (self.stream.total_in() - before) as usize;
            // A sync flush is done once all input is consumed and zlib left
            // spare output room after the call (nothing still pending).
            if consumed == self.input.len() && out.len() < out.capacity() {
                return Some(out);
            }
        }
    }
}

/// One iteration's fetch result, normalizing the vt100-grid sample and the
/// legacy capture-pane fork onto the same downstream publish/death logic.
enum CaptureOutcome {
    /// A renderable frame: ANSI content plus the (already reliability-filtered)
    /// cursor.
    Frame(String, Option<crate::tmux::PaneCursor>),
    /// The pane looks gone (dead channel, or an empty capture). Counts toward
    /// the dead-probe threshold before the connection closes.
    Dead,
}

static LIVE_CLIENT_COUNTER: AtomicU64 = AtomicU64::new(0);

pub async fn live_terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    debug!(target: "terminal.ws", session = %id, kind = "live", "ws route entered");
    if let Some(resp) = super::api::cityhall_block(&state) {
        return resp;
    }
    let instances = state.instances.read().await;
    let tmux_name = instances
        .iter()
        .find(|i| i.id == id)
        .map(|inst| crate::tmux::Session::resolve_name(&inst.id, &inst.title));
    drop(instances);

    let read_only = state.read_only;
    let shutdown = state.shutdown.clone();

    match tmux_name {
        Some(tmux_name) => ws
            .protocols(["aoe-auth"])
            .on_upgrade(move |socket| {
                handle_live_ws(socket, tmux_name, read_only, shutdown, LiveTransport::Grid)
            })
            .into_response(),
        None => {
            warn!(target: "terminal.ws", session = %id, kind = "live", "session not found, returning 404");
            (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response()
        }
    }
}

/// Index of the paired terminal a `live-ws` / ensure request targets.
/// Defaults to 0 (the historical single terminal); index >= 1 are the
/// additional web dashboard terminal tabs. See #2437.
#[derive(Deserialize, Default)]
pub struct TerminalIndexQuery {
    #[serde(default)]
    pub index: u32,
}

/// Live view for the paired host shell (TerminalSession). Mirrors the
/// paired PTY route's pane revival so a dead shell heals on reconnect.
pub async fn live_paired_terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<TerminalIndexQuery>,
) -> impl IntoResponse {
    live_shell_ws(
        ws,
        state,
        id,
        q.index,
        "paired-live",
        |state, id, inst, index| {
            Box::pin(super::pane::respawn_paired_if_dead(state, id, inst, index))
        },
    )
    .await
}

/// Live view for the paired in-container shell.
pub async fn live_container_terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Query(q): Query<TerminalIndexQuery>,
) -> impl IntoResponse {
    live_shell_ws(
        ws,
        state,
        id,
        q.index,
        "container-live",
        |state, id, inst, index| {
            Box::pin(super::pane::respawn_container_if_dead(
                state, id, inst, index,
            ))
        },
    )
    .await
}

type RespawnFn = for<'a> fn(
    &'a Arc<AppState>,
    &'a str,
    &'a crate::session::Instance,
    u32,
) -> std::pin::Pin<
    Box<dyn std::future::Future<Output = anyhow::Result<String>> + Send + 'a>,
>;

async fn live_shell_ws(
    ws: WebSocketUpgrade,
    state: Arc<AppState>,
    id: String,
    index: u32,
    kind: &'static str,
    respawn: RespawnFn,
) -> axum::response::Response {
    debug!(target: "terminal.ws", session = %id, kind = %kind, index, "ws route entered");
    // CityHall mode has no terminal surface; refuse the PTY relay outright so
    // the lockdown holds against a direct WS connection, not just a hidden UI.
    if let Some(resp) = super::api::cityhall_block(&state) {
        return resp;
    }
    if index > super::pane::MAX_TERMINAL_INDEX {
        warn!(target: "terminal.ws", session = %id, kind = %kind, index, "terminal index out of range");
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "Terminal index out of range",
        )
            .into_response();
    }
    let instances = state.instances.read().await;
    let inst = instances.iter().find(|i| i.id == id).cloned();
    drop(instances);

    let Some(inst) = inst else {
        warn!(target: "terminal.ws", session = %id, kind = %kind, "session not found, returning 404");
        return (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response();
    };

    let tmux_name = match respawn(&state, &id, &inst, index).await {
        Ok(name) => name,
        Err(e) => {
            warn!(target: "terminal.ws", session = %id, kind = %kind, "failed to revive shell: {}", e);
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to revive terminal",
            )
                .into_response();
        }
    };

    let read_only = state.read_only;
    let shutdown = state.shutdown.clone();
    ws.protocols(["aoe-auth"])
        .on_upgrade(move |socket| {
            handle_live_ws(
                socket,
                tmux_name,
                read_only,
                shutdown,
                LiveTransport::Snapshot,
            )
        })
        .into_response()
}

async fn handle_live_ws(
    mut socket: WebSocket,
    tmux_name: String,
    read_only: bool,
    shutdown: tokio_util::sync::CancellationToken,
    transport: LiveTransport,
) {
    match wait_for_tmux_ready(&tmux_name).await {
        PaneReadiness::Ready => {}
        PaneReadiness::Dead => {
            warn!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "pane dead, closing 4001");
            close_early(&mut socket, CLOSE_CODE_PTY_DEAD, "pty_dead").await;
            return;
        }
        PaneReadiness::NotReady => {
            warn!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "tmux not ready, closing 1013");
            close_early(&mut socket, CLOSE_CODE_TRY_AGAIN_LATER, "tmux_not_ready").await;
            return;
        }
    }

    let settings = Arc::new(LiveSettings {
        window_lines: AtomicUsize::new(DEFAULT_WINDOW_LINES),
        fast: AtomicBool::new(true),
        screen_rows: AtomicU64::new(0),
        screen_cols: AtomicU64::new(0),
        is_owner: AtomicBool::new(false),
        deflate: AtomicBool::new(false),
        patch: AtomicBool::new(false),
        force_full: AtomicBool::new(false),
        resize_settle_until_ms: AtomicU64::new(0),
    });
    // Identifies this connection in the cross-process size-owner lock (shared
    // with the web PTY attach and the native TUI via tmux user options).
    let owner_id = format!(
        "live-{}",
        LIVE_CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    // Wakes the capture loop out of its inter-capture sleep: after
    // dispatched input (echo latency) and after cadence/window changes.
    let nudge = Arc::new(tokio::sync::Notify::new());

    #[cfg(unix)]
    let config = crate::session::config::Config::load_or_warn();
    #[cfg(unix)]
    let clipboard_forward = clipboard_forward_enabled(config.tmux.clipboard, read_only);
    // The agent surface renders from the shared VT grid when one arms (the
    // native TUI preview shares it). Arming forks tmux and waits for the
    // forwarder, so it runs off the async runtime.
    #[cfg(unix)]
    let vt = if transport == LiveTransport::Grid && config.tmux.vt_live {
        let name = tmux_name.clone();
        tokio::task::spawn_blocking(move || {
            let deadline = crate::tmux::TmuxCommandDeadline::new();
            crate::tmux::vt::VtChannel::acquire_with_deadline(&name, &deadline)
        })
        .await
        .ok()
        .flatten()
    } else {
        None
    };
    #[cfg(not(unix))]
    let _ = transport;
    // Snapshot surfaces keep OSC 52 through a raw observer that builds no
    // grid. `pipe-pane` is exclusive, so it is only armed when no grid is.
    #[cfg(unix)]
    let osc52 = if clipboard_forward && vt.is_none() {
        crate::tmux::vt::Osc52Channel::acquire(&tmux_name)
    } else {
        None
    };

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Frames and pings funnel through one channel so the sender task is
    // the only writer on the socket.
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Message>(8);

    // Capture loop: fork capture-pane (+cursor) off the async runtime,
    // dedup, publish.
    let capture_settings = Arc::clone(&settings);
    let capture_nudge = Arc::clone(&nudge);
    let capture_tx = out_tx.clone();
    let capture_tmux = tmux_name.clone();
    let capture_owner = owner_id.clone();
    #[cfg(unix)]
    let capture_osc52 = osc52;
    #[cfg(unix)]
    let capture_vt = vt.clone();
    let capture_task = tokio::spawn(async move {
        #[cfg(unix)]
        let mut osc52_seen = capture_osc52
            .as_ref()
            .map_or(0, |source| source.clipboard_sequence());
        #[cfg(unix)]
        let mut vt_clipboard_seen = capture_vt.as_ref().map_or(0, |ch| ch.clipboard_sequence());
        // This connection's own change receiver: every viewer of the shared
        // grid gets one, so a change wakes all of them.
        #[cfg(unix)]
        let mut vt_rx = capture_vt.as_ref().map(|ch| ch.subscribe());
        // Pane count of the window, re-probed at most once per
        // PANE_COUNT_PROBE_INTERVAL while the grid path is in use.
        // `None` until tmux answers. An unprobed count must not read as a
        // single pane: the grid holds pane 0 alone, so believing that of a
        // split window would drop every other pane from the view. Unknown
        // takes the composited capture path, which is right for any count.
        #[cfg(unix)]
        let mut pane_count: (Option<u16>, Instant) =
            (None, Instant::now() - PANE_COUNT_PROBE_INTERVAL);
        #[cfg(unix)]
        let mut first_publish_wait_started: Option<Instant> = None;
        let mut last_published: Option<(String, Option<crate::tmux::PaneCursor>)> = None;
        // Announced on the first frame and whenever it flips, so a client can
        // report the transport rather than infer it.
        let mut announced_grid: Option<bool> = None;
        // Patch baseline: rows of the last message the client applied and its
        // scrollback depth, plus the running sequence number.
        let mut last_sent: Option<(Vec<String>, u32)> = None;
        let mut seq: u64 = 0;
        let mut stats = LiveStats::default();
        // Created on the first frame after the client advertises deflate;
        // lives for the connection so the dictionary spans frames.
        let mut deflater: Option<FrameDeflater> = None;
        let mut dead_probes: u32 = 0;
        let mut last_reassert = std::time::Instant::now() - REASSERT_MIN_INTERVAL;
        let mut reassert_guard = ReassertGuard::new(STUCK_REASSERT_RETRY);
        let mut last_heartbeat = std::time::Instant::now() - SIZE_OWNER_HEARTBEAT;
        let mut last_reclaim = std::time::Instant::now() - SIZE_OWNER_HEARTBEAT;
        loop {
            let sample_started = std::time::Instant::now();
            let lines = capture_settings.window_lines.load(Ordering::Relaxed);

            // Fetch tmux's authoritative rendered cells. A position-unreliable
            // cursor is treated as "no cursor" because the web frame has no
            // reliability channel and its renderer maps the row onto content.
            let outcome: CaptureOutcome;
            #[cfg(unix)]
            let mut grid_frame = false;
            #[cfg(unix)]
            {
                // The grid serves single-pane windows within its scrollback
                // depth; a split window is composited from capture-pane.
                let live_grid = capture_vt.as_ref().filter(|ch| ch.is_alive()).cloned();
                if live_grid.is_some() && pane_count.1.elapsed() >= PANE_COUNT_PROBE_INTERVAL {
                    let name = capture_tmux.clone();
                    // Advance the probe clock even on failure, or a tmux that
                    // cannot answer would be re-forked on every capture cycle
                    // instead of once a second. A failed probe keeps the last
                    // answer, which is `None` until one arrives.
                    let probed =
                        tokio::task::spawn_blocking(move || window_pane_count(&name)).await;
                    pane_count = (probed.ok().flatten().or(pane_count.0), Instant::now());
                }
                outcome = match live_grid {
                    Some(ch)
                        if pane_count.0 == Some(1)
                            && lines <= crate::tmux::vt::SCROLLBACK_LINES =>
                    {
                        grid_frame = true;
                        match tokio::task::spawn_blocking(move || {
                            let deadline = crate::tmux::TmuxCommandDeadline::new();
                            ch.sample_with_deadline(lines, &deadline)
                        })
                        .await
                        {
                            Ok((content, cursor)) => CaptureOutcome::Frame(content, cursor),
                            Err(_) => break,
                        }
                    }
                    _ => {
                        let name = capture_tmux.clone();
                        match tokio::task::spawn_blocking(move || {
                            crate::tmux::Session::from_name(&name)
                                .capture_window_composited_with_cursor(lines)
                        })
                        .await
                        {
                            Ok(Ok((content, cursor)))
                                if !content.is_empty()
                                    || cursor.as_ref().is_some_and(|c| c.position_reliable) =>
                            {
                                CaptureOutcome::Frame(content, cursor)
                            }
                            Ok(Ok(_)) => CaptureOutcome::Dead,
                            _ => break,
                        }
                    }
                };
            }
            #[cfg(not(unix))]
            {
                let name = capture_tmux.clone();
                outcome = match tokio::task::spawn_blocking(move || {
                    crate::tmux::Session::from_name(&name)
                        .capture_window_composited_with_cursor(lines)
                })
                .await
                {
                    Ok(Ok((content, cursor)))
                        if !content.is_empty()
                            || cursor.as_ref().is_some_and(|c| c.position_reliable) =>
                    {
                        CaptureOutcome::Frame(content, cursor)
                    }
                    Ok(Ok(_)) => CaptureOutcome::Dead,
                    _ => break,
                };
            }

            stats.samples += 1;
            stats.sample_micros += sample_started.elapsed().as_micros() as u64;

            match outcome {
                CaptureOutcome::Frame(content, cursor) => {
                    dead_probes = 0;
                    let cursor = cursor.filter(|c| c.position_reliable);
                    // Keep the size-owner lock alive while we hold it, and
                    // notice promptly if another client took over (then we
                    // demote ourselves to a read-only viewer).
                    if capture_settings.is_owner.load(Ordering::Relaxed)
                        && last_heartbeat.elapsed() >= SIZE_OWNER_HEARTBEAT
                    {
                        last_heartbeat = std::time::Instant::now();
                        let name = capture_tmux.clone();
                        let who = capture_owner.clone();
                        let still_owner = tokio::task::spawn_blocking(move || {
                            crate::tmux::Session::from_name(&name).refresh_size_owner(&who)
                        })
                        .await
                        .unwrap_or(false);
                        if !still_owner {
                            capture_settings.is_owner.store(false, Ordering::Relaxed);
                            let _ = capture_tx
                                .send(Message::Text(size_owner_json(false).into()))
                                .await;
                        }
                    }
                    // Auto-reclaim: a non-owner viewer re-CLAIMS (never
                    // steals) the lock once it goes vacant or stale, so when
                    // the current holder lets go (the TUI exits live mode,
                    // another web viewer disconnects) this client resumes
                    // ownership and its grid without the user re-tapping
                    // "take over". Gated to the fast cadence, i.e. a visible
                    // client at the live edge: a backgrounded PWA or a
                    // scrolled-up reader must not grab sizing the moment a
                    // desktop user releases it. While a live holder
                    // heartbeats, the claim fails cheaply; the throttle keeps
                    // that probe to one per heartbeat interval.
                    else if !capture_settings.is_owner.load(Ordering::Relaxed)
                        && capture_settings.fast.load(Ordering::Relaxed)
                        && last_reclaim.elapsed() >= SIZE_OWNER_HEARTBEAT
                    {
                        let cols = capture_settings.screen_cols.load(Ordering::Relaxed) as u16;
                        let rows = capture_settings.screen_rows.load(Ordering::Relaxed) as u16;
                        if cols > 0 && rows > 0 {
                            last_reclaim = std::time::Instant::now();
                            let name = capture_tmux.clone();
                            let who = capture_owner.clone();
                            let claimed = tokio::task::spawn_blocking(move || {
                                let session = crate::tmux::Session::from_name(&name);
                                if session.claim_size_owner(&who, SIZE_OWNER_TTL) {
                                    session.resize_window_if_owner(&who, cols, rows)
                                } else {
                                    false
                                }
                            })
                            .await
                            .unwrap_or(false);
                            if claimed {
                                capture_settings.is_owner.store(true, Ordering::Relaxed);
                                last_heartbeat = std::time::Instant::now();
                                let _ = capture_tx
                                    .send(Message::Text(size_owner_json(true).into()))
                                    .await;
                            }
                        }
                    }
                    // Only the owner drives the window size. Another writer
                    // (most commonly the TUI's preview sync) can resize the
                    // window out from under this viewer; the owner's capture
                    // lines then exceed its grid and render clipped, so the
                    // owner re-asserts. Non-owners render best-effort instead
                    // (the client hard-wraps drifted frames). Rate-limited as
                    // a guard against an unknown third writer.
                    if capture_settings.is_owner.load(Ordering::Relaxed) {
                        if let Some(c) = cursor.as_ref() {
                            let want_cols =
                                capture_settings.screen_cols.load(Ordering::Relaxed) as u16;
                            let want_rows =
                                capture_settings.screen_rows.load(Ordering::Relaxed) as u16;
                            let drifted = want_cols > 0
                                && want_rows > 0
                                && c.pane_width > 0
                                && (c.pane_width != want_cols || c.pane_height != want_rows);
                            let geom = DriftGeometry {
                                want_cols,
                                want_rows,
                                pane_cols: c.pane_width,
                                pane_rows: c.pane_height,
                            };
                            // Re-assert only for a genuine, not-yet-proven-stuck
                            // drift. Once a target proves unreachable (the pane
                            // didn't move after the last re-assert of the same
                            // geometry) the guard suppresses the repeat, so an
                            // off-by-one that survives the resize can't spin the
                            // 2s repaint loop forever (#2766). A real geometry
                            // change is a new tuple and re-asserts at once; the
                            // pane reaching target resets the guard below.
                            if drifted
                                && last_reassert.elapsed() >= REASSERT_MIN_INTERVAL
                                && reassert_guard.should_reassert(geom, std::time::Instant::now())
                            {
                                last_reassert = std::time::Instant::now();
                                warn!(
                                    target: "terminal.ws",
                                    tmux = %capture_tmux,
                                    kind = "live",
                                    pane_cols = c.pane_width,
                                    pane_rows = c.pane_height,
                                    want_cols,
                                    want_rows,
                                    "pane drifted from live owner's grid; re-asserting"
                                );
                                // Verified resize: the local is_owner flag is
                                // stale for up to a heartbeat after a steal,
                                // and a drift seen in that window IS the new
                                // owner's grid. Resizing unverified here would
                                // stomp it; instead demote on the spot.
                                let name = capture_tmux.clone();
                                let who = capture_owner.clone();
                                #[cfg(unix)]
                                let reassert_vt = capture_vt.clone();
                                let still_owner = tokio::task::spawn_blocking(move || {
                                    let owned = crate::tmux::Session::from_name(&name)
                                        .resize_window_if_owner(&who, want_cols, want_rows);
                                    #[cfg(unix)]
                                    if owned {
                                        if let Some(ch) = reassert_vt.as_ref() {
                                            let deadline = crate::tmux::TmuxCommandDeadline::new();
                                            ch.set_grid_size_with_deadline(
                                                want_cols, want_rows, &deadline,
                                            );
                                        }
                                    }
                                    owned
                                })
                                .await
                                .unwrap_or(false);
                                if still_owner {
                                    capture_settings
                                        .resize_settle_until_ms
                                        .store(live_now_ms() + RESIZE_SETTLE_MS, Ordering::Relaxed);
                                }
                                if !still_owner {
                                    capture_settings.is_owner.store(false, Ordering::Relaxed);
                                    let _ = capture_tx
                                        .send(Message::Text(size_owner_json(false).into()))
                                        .await;
                                }
                            }
                            if !drifted {
                                // Pane matches the grid; drop any stuck target so
                                // the next genuine drift re-asserts immediately.
                                reassert_guard.reset();
                            }
                        }
                    }
                    #[cfg(unix)]
                    {
                        let clipboard = match (capture_osc52.as_ref(), capture_vt.as_ref()) {
                            (Some(source), _) => {
                                source.refresh_owner_heartbeat();
                                source.clipboard_after(&mut osc52_seen)
                            }
                            (None, Some(ch)) => ch.clipboard_after(&mut vt_clipboard_seen),
                            (None, None) => None,
                        };
                        if clipboard_forward && capture_settings.is_owner.load(Ordering::Relaxed) {
                            if let Some(text) = clipboard {
                                if capture_tx
                                    .send(Message::Text(clipboard_json(&text).into()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                    // Post-resize settle: hold frames still at the old
                    // geometry so the client sees one clean repaint.
                    let settle_until = capture_settings
                        .resize_settle_until_ms
                        .load(Ordering::Relaxed);
                    if settle_until != 0 {
                        let want = (
                            capture_settings.screen_cols.load(Ordering::Relaxed) as u16,
                            capture_settings.screen_rows.load(Ordering::Relaxed) as u16,
                        );
                        let have = cursor
                            .as_ref()
                            .map_or(want, |c| (c.pane_width, c.pane_height));
                        if resize_settle_holds(live_now_ms(), settle_until, want, have) {
                            stats.settle_held += 1;
                            wait_for_next(
                                &capture_settings,
                                &capture_nudge,
                                #[cfg(unix)]
                                vt_rx.as_mut(),
                                sample_started,
                                #[cfg(unix)]
                                grid_frame,
                            )
                            .await;
                            continue;
                        }
                        capture_settings
                            .resize_settle_until_ms
                            .store(0, Ordering::Relaxed);
                    }
                    // Mid-bracket grid (the app is inside a synchronized-output
                    // repaint, or a reseed just copied tmux's half-drawn cells):
                    // wait for the close, which wakes the loop. The hold expires
                    // on its own if the app never closes the bracket.
                    #[cfg(unix)]
                    if grid_frame && capture_vt.as_ref().is_some_and(|ch| ch.sync_hold_active()) {
                        stats.sync_held += 1;
                        wait_for_next(
                            &capture_settings,
                            &capture_nudge,
                            vt_rx.as_mut(),
                            sample_started,
                            grid_frame,
                        )
                        .await;
                        continue;
                    }
                    // First publish from a freshly seeded grid: wait for the
                    // repaint the seed may have caught mid-flight to land and
                    // settle, so the opening frame is whole.
                    #[cfg(unix)]
                    if grid_frame && last_published.is_none() {
                        let fresh = capture_vt
                            .as_ref()
                            .is_some_and(|ch| ch.seed_age() < FRESH_SEED_MAX_AGE);
                        if fresh {
                            let started =
                                *first_publish_wait_started.get_or_insert_with(Instant::now);
                            let settled = capture_vt.as_ref().is_some_and(|ch| {
                                !ch.sync_hold_active()
                                    && ch.chunk_timing().is_some_and(|(since_last, _)| {
                                        since_last >= FIRST_PUBLISH_QUIET_MS
                                    })
                            });
                            if !settled
                                && started.elapsed()
                                    < Duration::from_millis(FIRST_PUBLISH_MAX_WAIT_MS)
                            {
                                tokio::time::sleep(Duration::from_millis(10)).await;
                                continue;
                            }
                        }
                    }
                    #[cfg(unix)]
                    if announced_grid != Some(grid_frame) {
                        announced_grid = Some(grid_frame);
                        if capture_tx
                            .send(Message::Text(transport_json(grid_frame).into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    let frame = (content, cursor);
                    // A resync republishes even when the frame is unchanged:
                    // the client dropped a patch and is showing a stale window
                    // it cannot recover from on its own.
                    let force_full = capture_settings.force_full.swap(false, Ordering::Relaxed);
                    if force_full || last_published.as_ref() != Some(&frame) {
                        seq += 1;
                        let lines = frame_lines(&frame.0);
                        let history = frame.1.as_ref().map_or(0, |c| c.history_size);
                        let alt = frame.1.as_ref().is_some_and(|c| c.alternate_on);
                        let patch = if capture_settings.patch.load(Ordering::Relaxed) && !force_full
                        {
                            last_sent.as_ref().and_then(|(prev, prev_history)| {
                                let shift = if alt {
                                    0
                                } else {
                                    history.saturating_sub(*prev_history) as usize
                                };
                                plan_patch(prev, &lines, shift).map(|changed| (changed, shift))
                            })
                        } else {
                            None
                        };
                        let json = match patch {
                            Some((changed, shift)) => {
                                stats.patches += 1;
                                patch_json(&changed, shift, seq, frame.1.as_ref())
                            }
                            None => frame_json(&frame.0, frame.1.as_ref(), seq),
                        };
                        last_sent = Some((lines.iter().map(|l| l.to_string()).collect(), history));
                        stats.publishes += 1;
                        stats.bytes += json.len() as u64;
                        if deflater.is_none() && capture_settings.deflate.load(Ordering::Relaxed) {
                            deflater = Some(FrameDeflater::new());
                        }
                        let msg = match deflater.as_mut() {
                            Some(d) => match d.frame(&json) {
                                Some(bytes) => Message::Binary(bytes.into()),
                                None => {
                                    // Corrupt compressor state (not expected):
                                    // degrade to text frames for the rest of
                                    // the connection; every client accepts
                                    // them regardless of caps.
                                    deflater = None;
                                    capture_settings.deflate.store(false, Ordering::Relaxed);
                                    Message::Text(json.into())
                                }
                            },
                            None => Message::Text(json.into()),
                        };
                        if capture_tx.send(msg).await.is_err() {
                            break; // socket gone
                        }
                        last_published = Some(frame);
                    }
                }
                CaptureOutcome::Dead => {
                    // Pane looks gone, or capture-pane returned an empty frame.
                    // Require a few consecutive misses before declaring death so
                    // a transient tmux hiccup doesn't kill the connection.
                    dead_probes += 1;
                    if dead_probes >= 3 {
                        let _ = capture_tx
                            .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                                code: CLOSE_CODE_PTY_DEAD,
                                reason: "pty_dead".into(),
                            })))
                            .await;
                        break;
                    }
                }
            }

            wait_for_next(
                &capture_settings,
                &capture_nudge,
                #[cfg(unix)]
                vt_rx.as_mut(),
                sample_started,
                #[cfg(unix)]
                grid_frame,
            )
            .await;
        }
        debug!(
            target: "terminal.ws",
            tmux = %capture_tmux,
            kind = "live",
            publishes = stats.publishes,
            patches = stats.patches,
            bytes = stats.bytes,
            samples = stats.samples,
            avg_sample_us = stats.sample_micros / stats.samples.max(1),
            settle_held = stats.settle_held,
            sync_held = stats.sync_held,
            "live capture loop ended"
        );
    });

    // Sender task: sole socket writer; also emits keepalive pings.
    let send_task = tokio::spawn(async move {
        let mut ping = tokio::time::interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ping.tick().await; // arm: first tick fires immediately otherwise
        loop {
            tokio::select! {
                msg = out_rx.recv() => {
                    match msg {
                        Some(Message::Close(frame)) => {
                            let _ = ws_sender.send(Message::Close(frame)).await;
                            break;
                        }
                        Some(msg) => {
                            if ws_sender.send(msg).await.is_err() {
                                break;
                            }
                        }
                        None => break,
                    }
                }
                _ = ping.tick() => {
                    if ws_sender.send(Message::Ping(vec![].into())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Recv loop: input bytes + control messages, until close/shutdown.
    loop {
        tokio::select! {
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Binary(data))) => {
                        // Only the size owner may type; a non-owner is a
                        // read-only viewer until it explicitly takes over.
                        if read_only
                            || data.is_empty()
                            || !settings.is_owner.load(Ordering::Relaxed)
                        {
                            continue;
                        }
                        let send_nudge = Arc::clone(&nudge);
                        let name = tmux_name.clone();
                        let bytes = data.to_vec();
                        // A live VT channel (ours or another surface's) is the
                        // pane's single input writer and bypasses tmux's key
                        // translation, so cursor keys are encoded for the
                        // pane's DECCKM state here. Otherwise input goes
                        // through tmux send-keys.
                        let _ = tokio::task::spawn_blocking(move || {
                            #[cfg(unix)]
                            if let Some(app_cursor) = crate::tmux::vt::input_mode(&name) {
                                let bytes = translate_cursor_keys(&bytes, app_cursor);
                                if crate::tmux::vt::try_send_input(&name, &bytes) {
                                    return;
                                }
                            }
                            let session = crate::tmux::Session::from_name(&name);
                            if let Err(e) = session.send_raw_bytes(&bytes) {
                                warn!(target: "terminal.ws", tmux = %name, kind = "live", "send_raw_bytes failed: {}", e);
                            }
                        })
                        .await;
                        // Capture the echo promptly rather than waiting out
                        // the current sleep.
                        send_nudge.notify_one();
                    }
                    Some(Ok(Message::Text(text))) => {
                        let Ok(control) = serde_json::from_str::<LiveControlMessage>(&text) else {
                            continue;
                        };
                        match control {
                            LiveControlMessage::Resize { cols, rows } => {
                                if cols == 0 || rows == 0 {
                                    continue;
                                }
                                settings.screen_rows.store(rows as u64, Ordering::Relaxed);
                                settings.screen_cols.store(cols as u64, Ordering::Relaxed);
                                // Never let the capture window clip the screen.
                                let floor = rows as usize;
                                if settings.window_lines.load(Ordering::Relaxed) < floor {
                                    settings.window_lines.store(floor, Ordering::Relaxed);
                                }
                                // Claim the cross-process size-owner lock; only
                                // the owner resizes the shared window. A
                                // non-owner keeps rendering best-effort at the
                                // owner's grid and shows a "take over" banner.
                                let name = tmux_name.clone();
                                let who = owner_id.clone();
                                #[cfg(unix)]
                                let resize_vt = vt.clone();
                                let owned = tokio::task::spawn_blocking(move || {
                                    let session = crate::tmux::Session::from_name(&name);
                                    let owned = session.claim_size_owner(&who, SIZE_OWNER_TTL)
                                        && session.resize_window_if_owner(&who, cols, rows);
                                    #[cfg(unix)]
                                    if owned {
                                        if let Some(ch) = resize_vt.as_ref() {
                                            let deadline = crate::tmux::TmuxCommandDeadline::new();
                                            ch.set_grid_size_with_deadline(cols, rows, &deadline);
                                        }
                                    }
                                    owned
                                })
                                .await
                                .unwrap_or(false);
                                if owned {
                                    settings.resize_settle_until_ms.store(
                                        live_now_ms() + RESIZE_SETTLE_MS,
                                        Ordering::Relaxed,
                                    );
                                }
                                settings.is_owner.store(owned, Ordering::Relaxed);
                                let _ = out_tx
                                    .send(Message::Text(size_owner_json(owned).into()))
                                    .await;
                                nudge.notify_one();
                            }
                            LiveControlMessage::Window { lines } => {
                                let floor = (settings.screen_rows.load(Ordering::Relaxed) as usize)
                                    .max(DEFAULT_WINDOW_LINES);
                                let clamped = lines.clamp(floor, MAX_WINDOW_LINES);
                                settings.window_lines.store(clamped, Ordering::Relaxed);
                                nudge.notify_one();
                            }
                            LiveControlMessage::Cadence { fast } => {
                                settings.fast.store(fast, Ordering::Relaxed);
                                if fast {
                                    nudge.notify_one();
                                }
                            }
                            LiveControlMessage::ClaimIfVacant => {
                                // A keyboard-open mobile pane intentionally
                                // postpones its first resize so it never sends
                                // keyboard-shrunk rows to tmux. It still needs
                                // an ownership decision before its gesture-
                                // bound input buffer can flush. Claim only an
                                // unheld or stale lock; unlike `claim`, this
                                // never takes control from another viewer.
                                let name = tmux_name.clone();
                                let who = owner_id.clone();
                                let owned = tokio::task::spawn_blocking(move || {
                                    crate::tmux::Session::from_name(&name)
                                        .claim_size_owner(&who, SIZE_OWNER_TTL)
                                })
                                .await
                                .unwrap_or(false);
                                settings.is_owner.store(owned, Ordering::Relaxed);
                                let _ = out_tx
                                    .send(Message::Text(size_owner_json(owned).into()))
                                    .await;
                                nudge.notify_one();
                            }
                            LiveControlMessage::Claim => {
                                // Explicit take-over: steal the lock even from
                                // a live holder, then size the window to our
                                // grid so this client renders correctly.
                                let name = tmux_name.clone();
                                let who = owner_id.clone();
                                let cols = settings.screen_cols.load(Ordering::Relaxed) as u16;
                                let rows = settings.screen_rows.load(Ordering::Relaxed) as u16;
                                #[cfg(unix)]
                                let claim_vt = vt.clone();
                                let owned = tokio::task::spawn_blocking(move || {
                                    let session = crate::tmux::Session::from_name(&name);
                                    if !session.steal_size_owner(&who) {
                                        return false;
                                    }
                                    if cols == 0 || rows == 0 {
                                        return true;
                                    }
                                    let owned = session.resize_window_if_owner(&who, cols, rows);
                                    #[cfg(unix)]
                                    if owned {
                                        if let Some(ch) = claim_vt.as_ref() {
                                            let deadline = crate::tmux::TmuxCommandDeadline::new();
                                            ch.set_grid_size_with_deadline(cols, rows, &deadline);
                                        }
                                    }
                                    owned
                                })
                                .await
                                .unwrap_or(false);
                                if owned && cols > 0 && rows > 0 {
                                    settings.resize_settle_until_ms.store(
                                        live_now_ms() + RESIZE_SETTLE_MS,
                                        Ordering::Relaxed,
                                    );
                                }
                                settings.is_owner.store(owned, Ordering::Relaxed);
                                let _ = out_tx
                                    .send(Message::Text(size_owner_json(owned).into()))
                                    .await;
                                nudge.notify_one();
                            }
                            LiveControlMessage::Caps { deflate, patch } => {
                                // Set-once: a client never revokes deflate (it
                                // has no way to reset its inflate stream), so
                                // ignore a false re-advertisement.
                                if deflate {
                                    settings.deflate.store(true, Ordering::Relaxed);
                                }
                                if patch {
                                    settings.patch.store(true, Ordering::Relaxed);
                                }
                            }
                            LiveControlMessage::Resync => {
                                settings.force_full.store(true, Ordering::Relaxed);
                                nudge.notify_one();
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {} // Ping/Pong handled by axum
                    Some(Err(e)) => {
                        debug!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "ws recv error: {}", e);
                        break;
                    }
                }
            }
            _ = shutdown.cancelled() => {
                let _ = out_tx
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: CLOSE_CODE_GOING_AWAY,
                        reason: "server shutdown".into(),
                    })))
                    .await;
                break;
            }
        }
    }

    capture_task.abort();
    drop(out_tx);
    let _ = send_task.await;

    // Release the size-owner lock if we held it. `release_size_owner` is a
    // no-op for a non-owner, and restores `window-size latest` once the lock
    // is vacant so a later full-size attach isn't pinned at phone dimensions.
    // With another live viewer still connected, the lock stays held by
    // whoever owns it; this disconnect doesn't disturb the survivor.
    {
        let name = tmux_name.clone();
        let who = owner_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            crate::tmux::Session::from_name(&name).release_size_owner(&who);
        })
        .await;
    }
    debug!(target: "terminal.ws", tmux = %tmux_name, kind = "live", "live ws closed");
}

/// Serialize one snapshot frame. `rows` (pane height) and `history`
/// (scrollback line count) ride at the top level: the client sizes its
/// virtual scroll spacer off `history` and slices the live screen off
/// the content's last `rows` lines, independent of cursor visibility.
/// Per-connection counters, logged when the capture loop ends.
#[derive(Default)]
struct LiveStats {
    /// Every message that carried content, full frames and patches alike.
    publishes: u64,
    patches: u64,
    bytes: u64,
    samples: u64,
    sample_micros: u64,
    settle_held: u64,
    sync_held: u64,
}

/// Number of panes in the session's first window, or `None` if tmux could
/// not answer.
#[cfg(unix)]
fn window_pane_count(tmux_name: &str) -> Option<u16> {
    let target = format!("{tmux_name}:^");
    let mut command = crate::tmux::tmux_command();
    command.args([
        "display-message",
        "-p",
        "-t",
        &target,
        "-F",
        "#{window_panes}",
    ]);
    let deadline = crate::tmux::TmuxCommandDeadline::new();
    let out = deadline.run(&mut command).ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

/// Sleep until the next reason to sample: the cadence ceiling (death
/// detection, size-owner heartbeat), an input nudge, or, when a grid drives a
/// screen-sized window, the grid's own change signal. A wide window means a
/// client reading scrollback, so it keeps the big-frame throttle even on the
/// grid path; and no cycle runs faster than FRAME_MIN_INTERVAL_MS so a spewing
/// pane cannot push more than ~60 frames a second.
async fn wait_for_next(
    settings: &LiveSettings,
    nudge: &tokio::sync::Notify,
    #[cfg(unix)] vt_rx: Option<&mut tokio::sync::watch::Receiver<()>>,
    sample_started: Instant,
    #[cfg(unix)] grid_driven: bool,
) {
    let screen = (settings.screen_rows.load(Ordering::Relaxed) as usize).max(DEFAULT_WINDOW_LINES);
    let small_window = settings.window_lines.load(Ordering::Relaxed) <= screen * 4;
    #[cfg(not(unix))]
    let grid_driven = false;
    // A backgrounded tab or an inactive terminal asks for the idle cadence.
    // The grid path has to honor that too: left armed, its change signal would
    // wake this loop on every repaint and re-render the window for a viewer
    // nobody is looking at, which on a phone is battery and data. The input
    // nudge is unaffected, so typed echo still wakes immediately.
    let fast = settings.fast.load(Ordering::Relaxed);
    let ms = if grid_driven && fast {
        GRID_CEILING_MS
    } else if fast && small_window {
        CAPTURE_INTERVAL_FAST_MS
    } else {
        CAPTURE_INTERVAL_IDLE_MS
    };
    let since = sample_started.elapsed();
    let floor = Duration::from_millis(FRAME_MIN_INTERVAL_MS);
    if since < floor {
        tokio::time::sleep(floor - since).await;
    }
    #[cfg(unix)]
    {
        let grid_arm = grid_driven && small_window && fast;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
            _ = nudge.notified() => {}
            _ = async {
                match vt_rx {
                    Some(rx) => {
                        if rx.changed().await.is_err() {
                            std::future::pending::<()>().await
                        }
                    }
                    None => std::future::pending::<()>().await,
                }
            }, if grid_arm => {}
        }
    }
    #[cfg(not(unix))]
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
        _ = nudge.notified() => {}
    }
}

/// The geometry, cursor and mode fields every frame and patch carries.
fn frame_meta(
    cursor: Option<&crate::tmux::PaneCursor>,
) -> serde_json::Map<String, serde_json::Value> {
    // The cursor is pane relative while composited content uses the window
    // grid. Emit window-relative coordinates and carry the same origin for
    // the client's inverse pointer mapping. Translating before emission also
    // keeps cursor painting correct in older clients that ignore the origin;
    // only their pointer mapping degrades. No pane rectangle means identity.
    let pane0 = cursor.and_then(|c| c.composite_pane0);
    let (origin_x, origin_y) = pane0.map_or((0, 0), |p| (p.left, p.top));
    let cursor_value = match cursor {
        Some(c) if c.visible => serde_json::json!({
            "x": c.x.saturating_add(origin_x),
            "y": c.y.saturating_add(origin_y),
        }),
        _ => serde_json::Value::Null,
    };
    let mut map = serde_json::Map::new();
    map.insert(
        "rows".into(),
        cursor.map(|c| c.pane_height).unwrap_or(0).into(),
    );
    map.insert(
        "history".into(),
        cursor.map(|c| c.history_size).unwrap_or(0).into(),
    );
    map.insert("cursor".into(), cursor_value);
    // Full-screen (alternate-screen) mouse apps have no capturable
    // scrollback; the client forwards the wheel to the app instead of
    // widening the capture window. `mouseSgr` picks the wire encoding.
    map.insert(
        "altScreen".into(),
        cursor.map(|c| c.alternate_on).unwrap_or(false).into(),
    );
    map.insert(
        "mouse".into(),
        cursor.map(|c| c.mouse_tracking).unwrap_or(false).into(),
    );
    map.insert(
        "mouseSgr".into(),
        cursor.map(|c| c.mouse_sgr).unwrap_or(false).into(),
    );
    map.insert(
        "pane0".into(),
        pane0.map_or(serde_json::Value::Null, |p| {
            serde_json::json!({
                "cols": p.width,
                "rows": p.height,
                "left": p.left,
                "top": p.top,
            })
        }),
    );
    map
}

/// Serialize a row patch (see the module doc).
fn patch_json(
    changed: &[(usize, &str)],
    shift: usize,
    seq: u64,
    cursor: Option<&crate::tmux::PaneCursor>,
) -> String {
    let mut map = frame_meta(cursor);
    map.insert("type".into(), "patch".into());
    map.insert("seq".into(), seq.into());
    map.insert("base".into(), (seq - 1).into());
    map.insert("shift".into(), shift.into());
    map.insert(
        "lines".into(),
        changed
            .iter()
            .map(|(i, row)| serde_json::json!([i, row]))
            .collect::<Vec<_>>()
            .into(),
    );
    serde_json::Value::Object(map).to_string()
}

fn frame_json(content: &str, cursor: Option<&crate::tmux::PaneCursor>, seq: u64) -> String {
    let mut map = frame_meta(cursor);
    map.insert("type".into(), "frame".into());
    map.insert("seq".into(), seq.into());
    map.insert("content".into(), content.into());
    serde_json::Value::Object(map).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn clipboard_forward_skips_read_only_viewers_and_the_disabled_mode() {
        use crate::session::config::TmuxSettingMode;

        assert!(clipboard_forward_enabled(TmuxSettingMode::Auto, false));
        assert!(clipboard_forward_enabled(TmuxSettingMode::Enabled, false));
        assert!(!clipboard_forward_enabled(TmuxSettingMode::Disabled, false));
        // A read-only viewer performed no action; its clipboard stays its own.
        assert!(!clipboard_forward_enabled(TmuxSettingMode::Auto, true));
        assert!(!clipboard_forward_enabled(TmuxSettingMode::Enabled, true));
    }

    #[test]
    fn clipboard_event_json_preserves_text() {
        let value: serde_json::Value =
            serde_json::from_str(&clipboard_json("line 1\n\"quoted\"")).unwrap();
        assert_eq!(value["type"], "clipboard");
        assert_eq!(value["text"], "line 1\n\"quoted\"");
    }

    fn geom(want: (u16, u16), pane: (u16, u16)) -> DriftGeometry {
        DriftGeometry {
            want_cols: want.0,
            want_rows: want.1,
            pane_cols: pane.0,
            pane_rows: pane.1,
        }
    }

    #[test]
    fn reassert_guard_suppresses_identical_stuck_target() {
        // #2766: an unreachable target (pane stuck one row short) must not
        // re-assert on a loop. First sight fires; the identical tuple is then
        // suppressed within the retry window.
        let mut g = ReassertGuard::new(STUCK_REASSERT_RETRY);
        let stuck = geom((115, 67), (115, 66));
        let t0 = Instant::now();
        assert!(g.should_reassert(stuck, t0), "first drift re-asserts");
        assert!(
            !g.should_reassert(stuck, t0 + Duration::from_secs(2)),
            "identical stuck target is suppressed"
        );
        assert!(
            !g.should_reassert(stuck, t0 + Duration::from_secs(20)),
            "still suppressed within the retry window"
        );
    }

    #[test]
    fn reassert_guard_allows_genuine_geometry_change() {
        let mut g = ReassertGuard::new(STUCK_REASSERT_RETRY);
        let t0 = Instant::now();
        assert!(g.should_reassert(geom((115, 67), (115, 66)), t0));
        // A real resize (new grid) is a different tuple: re-assert at once.
        assert!(
            g.should_reassert(geom((120, 70), (115, 66)), t0 + Duration::from_secs(1)),
            "changed target re-asserts immediately"
        );
    }

    #[test]
    fn reassert_guard_retries_after_window_and_after_reset() {
        let mut g = ReassertGuard::new(STUCK_REASSERT_RETRY);
        let stuck = geom((115, 67), (115, 66));
        let t0 = Instant::now();
        assert!(g.should_reassert(stuck, t0));
        assert!(!g.should_reassert(stuck, t0 + Duration::from_secs(10)));
        // Transient recovery: the same target is retried once past the window.
        assert!(
            g.should_reassert(stuck, t0 + STUCK_REASSERT_RETRY + Duration::from_secs(1)),
            "stuck target retries after the window"
        );
        // Reaching target resets the guard, so a later drift fires immediately.
        g.reset();
        // Without reset, t0+35s is 4s after the t0+31s re-assert (inside the
        // 30s window) and would be suppressed; reset clears it so it fires.
        assert!(g.should_reassert(stuck, t0 + Duration::from_secs(35)));
    }

    #[test]
    fn frame_json_includes_geometry_and_cursor() {
        let cases = [
            // Unsplit: `pane0` is null and the cursor is untouched.
            (None, (3, 7), serde_json::Value::Null),
            // Composited with pane 0 at the corner (a borderless split):
            // identity translation, but `pane0` rides with zero origin.
            (
                Some(crate::tmux::PaneGeom {
                    left: 0,
                    top: 0,
                    width: 37,
                    height: 46,
                }),
                (3, 7),
                serde_json::json!({
                    "cols": 37,
                    "rows": 46,
                    "left": 0,
                    "top": 0,
                }),
            ),
            // Composited with pane-border-status top: move the wire cursor
            // onto the window grid by pane 0's origin.
            (
                Some(crate::tmux::PaneGeom {
                    left: 2,
                    top: 1,
                    width: 37,
                    height: 46,
                }),
                (5, 8),
                serde_json::json!({
                    "cols": 37,
                    "rows": 46,
                    "left": 2,
                    "top": 1,
                }),
            ),
        ];
        for (pane0, want_cursor, want_pane0) in cases {
            let cursor = crate::tmux::PaneCursor {
                x: 3,
                y: 7,
                visible: true,
                pane_height: 46,
                history_size: 1200,
                pane_width: 74,
                alternate_on: false,
                mouse_tracking: false,
                mouse_sgr: false,
                mouse_all: false,
                position_reliable: true,
                composite_pane0: pane0,
            };
            let json = frame_json("hello\nworld", Some(&cursor), 1);
            let v: serde_json::Value = serde_json::from_str(&json).unwrap();
            assert_eq!(v["type"], "frame");
            assert_eq!(v["content"], "hello\nworld");
            assert_eq!(v["rows"], 46);
            assert_eq!(v["history"], 1200);
            assert_eq!(v["cursor"]["x"], want_cursor.0, "{pane0:?}");
            assert_eq!(v["cursor"]["y"], want_cursor.1, "{pane0:?}");
            assert_eq!(v["altScreen"], false);
            assert_eq!(v["mouse"], false);
            assert_eq!(v["mouseSgr"], false);
            assert_eq!(v["pane0"], want_pane0, "{pane0:?}");
        }
    }

    #[test]
    fn frame_json_reports_alt_screen_mouse_flags() {
        let cursor = crate::tmux::PaneCursor {
            x: 0,
            y: 0,
            visible: true,
            pane_height: 40,
            history_size: 0,
            pane_width: 80,
            alternate_on: true,
            mouse_tracking: true,
            mouse_sgr: false,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&frame_json("x", Some(&cursor), 1)).unwrap();
        assert_eq!(v["altScreen"], true);
        assert_eq!(v["mouse"], true);
        assert_eq!(v["mouseSgr"], false);
    }

    #[test]
    fn frame_json_hides_cursor_when_dectcem_off() {
        let cursor = crate::tmux::PaneCursor {
            x: 3,
            y: 7,
            visible: false,
            pane_height: 46,
            history_size: 0,
            pane_width: 74,
            alternate_on: false,
            mouse_tracking: false,
            mouse_sgr: false,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&frame_json("x", Some(&cursor), 1)).unwrap();
        assert!(v["cursor"].is_null());
        assert_eq!(v["rows"], 46);
    }

    #[test]
    fn frame_json_null_cursor() {
        let v: serde_json::Value = serde_json::from_str(&frame_json("x", None, 1)).unwrap();
        assert!(v["cursor"].is_null());
        assert_eq!(v["rows"], 0);
    }

    #[test]
    fn control_messages_parse() {
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"resize","cols":74,"rows":46}"#).unwrap();
        assert!(matches!(
            m,
            LiveControlMessage::Resize { cols: 74, rows: 46 }
        ));
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"window","lines":800}"#).unwrap();
        assert!(matches!(m, LiveControlMessage::Window { lines: 800 }));
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"cadence","fast":false}"#).unwrap();
        assert!(matches!(m, LiveControlMessage::Cadence { fast: false }));
        let m: LiveControlMessage = serde_json::from_str(r#"{"type":"claim"}"#).unwrap();
        assert!(matches!(m, LiveControlMessage::Claim));
        let m: LiveControlMessage = serde_json::from_str(r#"{"type":"claim_if_vacant"}"#).unwrap();
        assert!(matches!(m, LiveControlMessage::ClaimIfVacant));
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"caps","deflate":true}"#).unwrap();
        assert!(matches!(
            m,
            LiveControlMessage::Caps {
                deflate: true,
                patch: false
            }
        ));
        let m: LiveControlMessage =
            serde_json::from_str(r#"{"type":"caps","deflate":true,"patch":true}"#).unwrap();
        assert!(matches!(
            m,
            LiveControlMessage::Caps {
                deflate: true,
                patch: true
            }
        ));
        let m: LiveControlMessage = serde_json::from_str(r#"{"type":"resync"}"#).unwrap();
        assert!(matches!(m, LiveControlMessage::Resync));
    }

    /// Feed the deflater's binary payloads through one raw-inflate stream
    /// (what the browser's `DecompressionStream("deflate-raw")` does) and
    /// re-split the plaintext on the u32-LE length prefixes.
    fn inflate_records(chunks: &[&[u8]]) -> Vec<String> {
        let mut stream = flate2::Decompress::new(false);
        let mut plain: Vec<u8> = Vec::new();
        for chunk in chunks {
            let mut consumed = 0usize;
            loop {
                plain.reserve(4096);
                let before = stream.total_in();
                stream
                    .decompress_vec(
                        &chunk[consumed..],
                        &mut plain,
                        flate2::FlushDecompress::Sync,
                    )
                    .unwrap();
                consumed += (stream.total_in() - before) as usize;
                if consumed == chunk.len() && plain.len() < plain.capacity() {
                    break;
                }
            }
        }
        let mut records = Vec::new();
        let mut pos = 0usize;
        while plain.len() - pos >= 4 {
            let len = u32::from_le_bytes(plain[pos..pos + 4].try_into().unwrap()) as usize;
            assert!(plain.len() - pos - 4 >= len, "truncated record");
            records.push(String::from_utf8(plain[pos + 4..pos + 4 + len].to_vec()).unwrap());
            pos += 4 + len;
        }
        assert_eq!(pos, plain.len(), "trailing garbage after last record");
        records
    }

    #[test]
    fn frame_deflater_roundtrips_and_shares_dictionary_across_frames() {
        let screen: String = (0..50)
            .map(|i| format!("\x1b[38;5;208mline {i} with some agent output text\x1b[0m\n"))
            .collect();
        let frame1 = frame_json(&screen, None, 1);
        // Frame 2: same screen scrolled by one line, the shape a scroll burst
        // produces. Nearly all of its content already sits in the dictionary.
        let scrolled = format!(
            "{}\x1b[38;5;208mline 50 with some agent output text\x1b[0m\n",
            screen.split_once('\n').unwrap().1
        );
        let frame2 = frame_json(&scrolled, None, 2);

        let mut d = FrameDeflater::new();
        let c1 = d.frame(&frame1).unwrap();
        let c2 = d.frame(&frame2).unwrap();

        let records = inflate_records(&[&c1, &c2]);
        assert_eq!(records, vec![frame1.clone(), frame2.clone()]);
        // The cross-frame dictionary is the point: the second frame must
        // compress far below what standalone compression of ~repeated text
        // achieves. 10x is a loose floor; in practice it is much higher.
        assert!(
            c2.len() < frame2.len() / 10,
            "no dictionary gain: {} vs {}",
            c2.len(),
            frame2.len()
        );
    }

    fn owned(rows: &[&str]) -> Vec<String> {
        rows.iter().map(|r| r.to_string()).collect()
    }

    #[test]
    fn plan_patch_lists_changed_rows_after_shift_and_falls_back_to_full_frames() {
        // (prev, next, shift, expected)
        type Case<'a> = (
            &'a [&'a str],
            &'a [&'a str],
            usize,
            Option<Vec<(usize, &'a str)>>,
        );
        let cases: &[Case] = &[
            // Identical windows: an empty patch (cursor/flags still ride).
            (
                &["a", "b", "c", "d"],
                &["a", "b", "c", "d"],
                0,
                Some(vec![]),
            ),
            // One row changed in place (a spinner tick).
            (
                &["a", "b", "c", "d"],
                &["a", "B", "c", "d"],
                0,
                Some(vec![(1, "B")]),
            ),
            // History grew by one: the window slid up, only the new tail row
            // is different once aligned.
            (
                &["a", "b", "c", "d"],
                &["b", "c", "d", "e"],
                1,
                Some(vec![(3, "e")]),
            ),
            // Too many rows changed: a full frame is smaller.
            (&["a", "b", "c", "d"], &["w", "x", "y", "d"], 0, None),
            // Window height changed (resize / wider capture): full frame.
            (&["a", "b", "c"], &["a", "b", "c", "d"], 0, None),
            // Shift past the whole window: everything is new.
            (&["a", "b"], &["y", "z"], 5, None),
            // Empty windows never patch.
            (&[], &[], 0, None),
        ];
        for (prev, next, shift, expected) in cases {
            assert_eq!(
                &plan_patch(&owned(prev), next, *shift),
                expected,
                "{prev:?} -> {next:?} shift {shift}"
            );
        }
    }

    #[test]
    fn frame_lines_drops_only_the_terminating_newline() {
        assert_eq!(frame_lines("a\nb\n"), vec!["a", "b"]);
        assert_eq!(frame_lines("a\n\n"), vec!["a", ""]);
        assert_eq!(frame_lines("a"), vec!["a"]);
        assert_eq!(frame_lines(""), vec![""]);
    }

    #[test]
    fn translate_cursor_keys_rewrites_bare_arrows_only_in_application_mode() {
        let normal = b"\x1b[A\x1b[D";
        assert_eq!(&*translate_cursor_keys(normal, false), normal);
        assert_eq!(&*translate_cursor_keys(normal, true), b"\x1bOA\x1bOD");
        // Home/End follow; modified arrows and other CSI stay verbatim.
        assert_eq!(
            &*translate_cursor_keys(b"x\x1b[H\x1b[1;5A\x1b[3~\x1b[F", true),
            b"x\x1bOH\x1b[1;5A\x1b[3~\x1bOF"
        );
        // A trailing partial sequence is passed through untouched.
        assert_eq!(&*translate_cursor_keys(b"\x1b[", true), b"\x1b[");
    }

    #[test]
    fn resize_settle_holds_only_mismatched_geometry_inside_the_window() {
        assert!(resize_settle_holds(100, 400, (80, 24), (120, 40)));
        assert!(!resize_settle_holds(100, 400, (80, 24), (80, 24)));
        assert!(!resize_settle_holds(500, 400, (80, 24), (120, 40)));
    }

    #[test]
    fn patch_json_carries_rows_shift_sequence_and_frame_meta() {
        let cursor = crate::tmux::PaneCursor {
            x: 2,
            y: 3,
            visible: true,
            pane_height: 4,
            history_size: 9,
            pane_width: 40,
            alternate_on: true,
            mouse_tracking: true,
            mouse_sgr: true,
            mouse_all: false,
            position_reliable: true,
            composite_pane0: None,
        };
        let v: serde_json::Value =
            serde_json::from_str(&patch_json(&[(1, "B"), (3, "e")], 1, 7, Some(&cursor))).unwrap();
        assert_eq!(v["type"], "patch");
        assert_eq!(v["seq"], 7);
        assert_eq!(v["base"], 6);
        assert_eq!(v["shift"], 1);
        assert_eq!(v["lines"], serde_json::json!([[1, "B"], [3, "e"]]));
        assert_eq!(v["rows"], 4);
        assert_eq!(v["history"], 9);
        assert_eq!(v["cursor"], serde_json::json!({"x": 2, "y": 3}));
        assert_eq!(v["altScreen"], true);
        let f: serde_json::Value =
            serde_json::from_str(&frame_json("x\n", Some(&cursor), 8)).unwrap();
        assert_eq!(f["type"], "frame");
        assert_eq!(f["seq"], 8);
        assert_eq!(f["content"], "x\n");
    }
}
