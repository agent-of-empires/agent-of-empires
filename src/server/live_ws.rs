//! Capture-snapshot live view for the web dashboard (mobile).
//!
//! Mirrors the TUI's live-send architecture instead of the PTY attach
//! relay: the server polls `tmux capture-pane` (cursor folded into the
//! same fork) and pushes ANSI snapshot frames over the WebSocket;
//! browser input comes back as raw bytes and is delivered via
//! `tmux send-keys -H`. No PTY, no `tmux attach`, no SIGSTOP pause:
//! scrollback is just a bigger capture window the client renders and
//! scrolls natively, and the agent keeps running while the user reads.
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
//!   `{"type":"size_owner","is_owner":bool}`: whether this client holds
//!     the session's size-owner lock. Only the owner resizes the shared
//!     tmux window and may type; a non-owner renders best-effort at the
//!     owner's grid and shows a "take over" affordance. A visible
//!     non-owner at fast cadence auto-reclaims the lock (claim, never
//!     steal) once the holder releases it, so ownership returns without
//!     another "take over" tap.
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
//!   `{"type":"caps","deflate":bool}`: client capability advertisement.
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
/// Minimum gap between samples when a vt channel drives the loop on output
/// (the grid-change watch arm). The channel wakes us the instant the grid
/// changes, so the cadence above is no longer the latency floor; this caps a
/// spewing pane at ~60fps instead of letting it busy-loop the socket. Only the
/// live-edge (small-window) path is event-driven, so this never applies while
/// a client reads scrollback.
const FRAME_MIN_INTERVAL_MS: u64 = 16;
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
    /// frame delivery to the compressed binary stream.
    #[serde(rename = "caps")]
    Caps {
        #[serde(default)]
        deflate: bool,
    },
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
}

/// JSON control frame telling the client whether it currently owns the
/// session's size (and may resize/type) or is a read-only viewer.
fn size_owner_json(is_owner: bool) -> String {
    serde_json::json!({ "type": "size_owner", "is_owner": is_owner }).to_string()
}

fn clipboard_json(text: &str) -> String {
    serde_json::json!({ "type": "clipboard", "text": text }).to_string()
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
            .on_upgrade(move |socket| handle_live_ws(socket, tmux_name, read_only, shutdown))
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
        .on_upgrade(move |socket| handle_live_ws(socket, tmux_name, read_only, shutdown))
        .into_response()
}

async fn handle_live_ws(
    mut socket: WebSocket,
    tmux_name: String,
    read_only: bool,
    shutdown: tokio_util::sync::CancellationToken,
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

    // Acquire the shared vt100 channel for this pane (armed once, shared with
    // the native TUI preview and any other web viewer). `Some` => render from
    // the in-process grid and inject input over the socket; `None` (tmux < 3.4,
    // arm failure, non-unix, or `[tmux] vt_live` off) => fall back to the
    // capture-pane loop and send-keys. Held for the whole connection so the
    // channel stays alive. The setting is read per connection and gates
    // *arming*, not *reuse*: while other holders keep a channel alive it is
    // the pane's single input writer, so a new connection must join it (or
    // its send-keys would race the socket); the fallback only becomes real
    // once the last holder drops and the channel dies.
    #[cfg(unix)]
    let config = crate::session::config::Config::load_or_warn();
    #[cfg(unix)]
    let vt = if config.tmux.vt_live {
        crate::tmux::vt::VtChannel::acquire(&tmux_name)
    } else {
        crate::tmux::vt::VtChannel::reuse(&tmux_name)
    };
    #[cfg(unix)]
    let clipboard_forward = clipboard_forward_enabled(config.tmux.clipboard, read_only);

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
    let capture_vt = vt.clone();
    let capture_task = tokio::spawn(async move {
        // This connection's own change receiver: every viewer of the shared
        // channel gets one, so a grid change wakes all of them (not just one).
        #[cfg(unix)]
        let mut vt_rx = capture_vt.as_ref().map(|ch| ch.subscribe());
        #[cfg(unix)]
        let mut clipboard_rx = capture_vt.as_ref().map(|ch| ch.subscribe_clipboard());
        let mut last_published: Option<(String, Option<crate::tmux::PaneCursor>)> = None;
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

            // Fetch one frame: from the shared vt100 grid when a channel is
            // armed (no fork), else the legacy capture-pane fork. A
            // position-unreliable cursor (capture path: the pane scrolled
            // between the two probes) is treated as "no cursor"; the web frame
            // has no `position_reliable` channel and its renderer maps the
            // cursor row onto the content, so painting it would land wrong.
            let outcome: CaptureOutcome;
            #[cfg(unix)]
            {
                outcome = match &capture_vt {
                    // The VT grid intentionally retains tmux's default
                    // 2,000-line scrollback for fast live-edge snapshots.
                    // The web protocol permits a bounded 4,000-line reading
                    // window, though, so never let that smaller cache make
                    // retained tmux history disappear. Deep reads take the
                    // authoritative capture-pane path below; the normal
                    // screen-sized live tail stays on the event-driven grid.
                    Some(ch) if ch.is_alive() && lines <= crate::tmux::vt::SCROLLBACK_LINES => {
                        let ch = ch.clone();
                        match tokio::task::spawn_blocking(move || ch.sample(lines)).await {
                            Ok((content, cursor)) => CaptureOutcome::Frame(content, cursor),
                            Err(_) => break,
                        }
                    }
                    // No channel, or the held channel's forwarder has died (a
                    // pipe failure, not necessarily a dead pane): fall back to
                    // the legacy capture-pane fork rather than black-holing.
                    // If the pane is truly gone the fork returns empty -> Dead
                    // and the connection still closes; if only the pipe died
                    // the pane keeps rendering, so we recover. Input mirrors
                    // this by gating `armed` on `is_alive` below.
                    _ => {
                        let name = capture_tmux.clone();
                        match tokio::task::spawn_blocking(move || {
                            crate::tmux::Session::from_name(&name).capture_pane_with_cursor(lines)
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
                    crate::tmux::Session::from_name(&name).capture_pane_with_cursor(lines)
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
                                    session.resize_window(cols, rows);
                                    true
                                } else {
                                    false
                                }
                            })
                            .await
                            .unwrap_or(false);
                            if claimed {
                                capture_settings.is_owner.store(true, Ordering::Relaxed);
                                last_heartbeat = std::time::Instant::now();
                                #[cfg(unix)]
                                if let Some(ch) = &capture_vt {
                                    ch.set_grid_size(cols, rows);
                                }
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
                                let still_owner = tokio::task::spawn_blocking(move || {
                                    crate::tmux::Session::from_name(&name)
                                        .resize_window_if_owner(&who, want_cols, want_rows)
                                })
                                .await
                                .unwrap_or(false);
                                if still_owner {
                                    // Track the new geometry in the shared grid
                                    // immediately so the parser doesn't wait out
                                    // `reconcile_size` after the resize.
                                    #[cfg(unix)]
                                    if let Some(ch) = &capture_vt {
                                        ch.set_grid_size(want_cols, want_rows);
                                    }
                                } else {
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
                    if let Some(rx) = clipboard_rx.as_mut() {
                        if rx.has_changed().unwrap_or(false) {
                            let clipboard = rx.borrow_and_update().clone();
                            if clipboard_forward
                                && capture_settings.is_owner.load(Ordering::Relaxed)
                            {
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
                    }
                    let frame = (content, cursor);
                    if last_published.as_ref() != Some(&frame) {
                        let json = frame_json(&frame.0, frame.1.as_ref());
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
                    // Pane looks gone (dead vt channel, or an empty capture).
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

            // Fast cadence only makes sense for screen-sized windows. A
            // wide window means a client reading scrollback; the new
            // client requests idle cadence itself, but cap it here too so
            // a stale PWA bundle (which spoke the retired hold protocol
            // and never lowers cadence) cannot keep the server pushing
            // multi-thousand-line frames at 20/s.
            let screen = (capture_settings.screen_rows.load(Ordering::Relaxed) as usize)
                .max(DEFAULT_WINDOW_LINES);
            let small_window = capture_settings.window_lines.load(Ordering::Relaxed) <= screen * 4;
            let ms = if capture_settings.fast.load(Ordering::Relaxed) && small_window {
                CAPTURE_INTERVAL_FAST_MS
            } else {
                CAPTURE_INTERVAL_IDLE_MS
            };

            // Rate cap: hold each cycle to at least FRAME_MIN so a pane spewing
            // output (the grid-change arm fires back-to-back) is bounded to
            // ~60fps rather than busy-looping. A nudge or grid bump that lands
            // during this pad is retained (the watch keeps its version, the
            // nudge keeps a permit), so the wait below returns immediately and
            // no wake is lost.
            let since = sample_started.elapsed();
            let floor = Duration::from_millis(FRAME_MIN_INTERVAL_MS);
            if since < floor {
                tokio::time::sleep(floor - since).await;
            }

            // Wait for the next reason to sample. `ms` is the ceiling (death
            // detection, size-owner heartbeat); a nudge wakes us for typed
            // echo; and when a vt channel drives a live-edge window it wakes us
            // the instant the grid changes, so output latency is one socket
            // hop, not a cadence tick. The grid arm is gated to `small_window`
            // so a client reading scrollback keeps the big-frame throttle.
            #[cfg(unix)]
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                _ = capture_nudge.notified() => {}
                _ = async {
                    match &mut vt_rx {
                        // `changed()` resolves on the next grid bump, or
                        // immediately if one happened since the last wait, so
                        // output between waits is never missed. Err (sender
                        // gone) can't happen while we hold the channel Arc;
                        // park if it ever does rather than spin.
                        Some(rx) => {
                            if rx.changed().await.is_err() {
                                std::future::pending::<()>().await
                            }
                        }
                        None => std::future::pending::<()>().await,
                    }
                }, if small_window => {}
            }
            #[cfg(not(unix))]
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(ms)) => {}
                _ = capture_nudge.notified() => {}
            }
        }
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
                        // When a vt channel is armed, all pane input goes through
                        // its socket (single-writer; mixing with send-keys would
                        // interleave two writers on the pty input). Otherwise fork
                        // send-keys as before. The browser already sends raw bytes,
                        // so no key encoding is needed on this path.
                        // Gate on `is_alive`, not just `is_some`: a held
                        // channel whose forwarder has died must fall back to
                        // send-keys, or input would be written into a dead
                        // socket and silently dropped (capture falls back the
                        // same way above).
                        #[cfg(unix)]
                        let armed = vt.as_ref().is_some_and(|ch| ch.is_alive());
                        #[cfg(not(unix))]
                        let armed = false;
                        if armed {
                            #[cfg(unix)]
                            {
                                let name = tmux_name.clone();
                                let bytes = data.to_vec();
                                let _ = tokio::task::spawn_blocking(move || {
                                    crate::tmux::vt::try_send_input(&name, &bytes);
                                })
                                .await;
                            }
                        } else {
                            let name = tmux_name.clone();
                            let bytes = data.to_vec();
                            // Off-runtime: send-keys forks a subprocess.
                            let _ = tokio::task::spawn_blocking(move || {
                                // A live channel armed by another holder (the
                                // TUI, an older connection) is the pane's
                                // single input writer; route through it
                                // rather than racing it with send-keys.
                                // Mirrors the TUI's `dispatch_via_fork`.
                                #[cfg(unix)]
                                if crate::tmux::vt::try_send_input(&name, &bytes) {
                                    return;
                                }
                                let session = crate::tmux::Session::from_name(&name);
                                if let Err(e) = session.send_raw_bytes(&bytes) {
                                    warn!(target: "terminal.ws", tmux = %name, kind = "live", "send_raw_bytes failed: {}", e);
                                }
                            })
                            .await;
                        }
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
                                let owned = tokio::task::spawn_blocking(move || {
                                    let session = crate::tmux::Session::from_name(&name);
                                    if session.claim_size_owner(&who, SIZE_OWNER_TTL) {
                                        session.resize_window(cols, rows);
                                        true
                                    } else {
                                        false
                                    }
                                })
                                .await
                                .unwrap_or(false);
                                settings.is_owner.store(owned, Ordering::Relaxed);
                                #[cfg(unix)]
                                if owned {
                                    if let Some(ch) = &vt {
                                        ch.set_grid_size(cols, rows);
                                    }
                                }
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
                                let owned = tokio::task::spawn_blocking(move || {
                                    let session = crate::tmux::Session::from_name(&name);
                                    if session.steal_size_owner(&who) {
                                        if cols > 0 && rows > 0 {
                                            session.resize_window(cols, rows);
                                        }
                                        true
                                    } else {
                                        false
                                    }
                                })
                                .await
                                .unwrap_or(false);
                                settings.is_owner.store(owned, Ordering::Relaxed);
                                #[cfg(unix)]
                                if owned {
                                    if let Some(ch) = &vt {
                                        ch.set_grid_size(cols, rows);
                                    }
                                }
                                let _ = out_tx
                                    .send(Message::Text(size_owner_json(owned).into()))
                                    .await;
                                nudge.notify_one();
                            }
                            LiveControlMessage::Caps { deflate } => {
                                // Set-once: a client never revokes deflate (it
                                // has no way to reset its inflate stream), so
                                // ignore a false re-advertisement.
                                if deflate {
                                    settings.deflate.store(true, Ordering::Relaxed);
                                }
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
fn frame_json(content: &str, cursor: Option<&crate::tmux::PaneCursor>) -> String {
    let cursor_value = match cursor {
        Some(c) if c.visible => serde_json::json!({
            "x": c.x,
            "y": c.y,
        }),
        _ => serde_json::Value::Null,
    };
    serde_json::json!({
        "type": "frame",
        "content": content,
        "rows": cursor.map(|c| c.pane_height).unwrap_or(0),
        "history": cursor.map(|c| c.history_size).unwrap_or(0),
        "cursor": cursor_value,
        // Full-screen (alternate-screen) mouse apps have no capturable
        // scrollback; the client forwards the wheel to the app instead of
        // widening the capture window. `mouseSgr` picks the wire encoding.
        "altScreen": cursor.map(|c| c.alternate_on).unwrap_or(false),
        "mouse": cursor.map(|c| c.mouse_tracking).unwrap_or(false),
        "mouseSgr": cursor.map(|c| c.mouse_sgr).unwrap_or(false),
    })
    .to_string()
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
            composite_pane0: None,
        };
        let json = frame_json("hello\nworld", Some(&cursor));
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], "frame");
        assert_eq!(v["content"], "hello\nworld");
        assert_eq!(v["rows"], 46);
        assert_eq!(v["history"], 1200);
        assert_eq!(v["cursor"]["x"], 3);
        assert_eq!(v["cursor"]["y"], 7);
        assert_eq!(v["altScreen"], false);
        assert_eq!(v["mouse"], false);
        assert_eq!(v["mouseSgr"], false);
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
        let v: serde_json::Value = serde_json::from_str(&frame_json("x", Some(&cursor))).unwrap();
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
        let v: serde_json::Value = serde_json::from_str(&frame_json("x", Some(&cursor))).unwrap();
        assert!(v["cursor"].is_null());
        assert_eq!(v["rows"], 46);
    }

    #[test]
    fn frame_json_null_cursor() {
        let v: serde_json::Value = serde_json::from_str(&frame_json("x", None)).unwrap();
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
        assert!(matches!(m, LiveControlMessage::Caps { deflate: true }));
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
        let frame1 = frame_json(&screen, None);
        // Frame 2: same screen scrolled by one line, the shape a scroll burst
        // produces. Nearly all of its content already sits in the dictionary.
        let scrolled = format!(
            "{}\x1b[38;5;208mline 50 with some agent output text\x1b[0m\n",
            screen.split_once('\n').unwrap().1
        );
        let frame2 = frame_json(&scrolled, None);

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
}
