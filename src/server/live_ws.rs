//! Live terminal view for the web dashboard.
//!
//! One WebSocket per viewer on `/sessions/{id}/live-ws`. There is no PTY and no
//! `tmux attach`: the server publishes rendered windows and the client scrolls them.
//!
//! Server to client, JSON text frames, each carrying a monotonic `seq` plus the shared
//! geometry block `rows`, `history`, `cursor` (`{"x","y"}` in window coordinates, or
//! null), `altScreen`, `mouse`, `mouseSgr` and `pane0`
//! (`{"cols","rows","left","top"}`, null unless the window is composited from a split):
//!   - `{"type":"frame","content":"<ANSI text>",..}`: the whole window, history lines
//!     first and the live screen as the last `rows` lines.
//!   - `{"type":"patch","base":..,"shift":k,"lines":[[i,"<ANSI>"],..],..}`: sent in a
//!     frame's place once the client advertises `caps.patch` and few rows changed. The
//!     client drops its first `shift` rows, appends `shift` blank ones, then replaces
//!     the listed rows. `base` names the `seq` it applies to.
//!   - `{"type":"size_owner","is_owner":bool}`: only the owner resizes the shared tmux
//!     window and may type; the lock lives in tmux user options, so the web view and the
//!     native TUI honor the same owner.
//!   - `{"type":"transport","grid":bool}`: `false` is the capture fallback, which cannot
//!     suppress a half-drawn repaint.
//!   - `{"type":"clipboard","text":"..."}`: an OSC 52 write by the pane.
//!
//! Client to server: binary frames are raw pane input (dropped for a read-only or
//! non-owner client), and the control messages are `resize` (claim the size lock and
//! size the window), `claim` / `claim_if_vacant` (take over from a non-owner), `window`
//! (capture window in lines), `cadence` (fast at the live edge, idle otherwise),
//! `resync` (lost patch continuity, send a full frame) and
//! `{"type":"caps","deflate":bool,"patch":bool}`. With `deflate`, frame messages switch
//! from text to binary: one connection-lifetime raw-deflate stream, sync-flushed per
//! frame, carrying `u32-LE length || frame JSON` records, so consecutive near-identical
//! frames compress against a shared dictionary. `size_owner` and close frames stay text.

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

/// Capture cadence while the client is at the live edge.
const CAPTURE_INTERVAL_FAST_MS: u64 = 50;
/// Cadence while the client reads scrollback or is backgrounded.
const CAPTURE_INTERVAL_IDLE_MS: u64 = 250;
/// Minimum gap between snapshot samples.
const FRAME_MIN_INTERVAL_MS: u64 = 16;
/// Wait ceiling while a VT channel drives the loop.
const GRID_CEILING_MS: u64 = 250;
/// After the owner resizes the window, frames whose pane geometry still
/// disagrees with the requested grid are withheld for this long, so the client
/// sees one clean repaint instead of a clear, a half-draw, and a settle.
const RESIZE_SETTLE_MS: u64 = 300;
/// Gap between retries of a VT reseed a resize could not land.
const GRID_RESYNC_RETRY: Duration = Duration::from_millis(120);
/// A freshly armed channel seeded from `capture-pane`, which cannot tell whether the app
/// was mid-repaint.
const FIRST_PUBLISH_QUIET_MS: u64 = 30;
/// Upper bound on that first-publish wait, so an idle pane still paints.
const FIRST_PUBLISH_MAX_WAIT_MS: u64 = 150;
/// A channel older than this was seeded long before this viewer connected;
/// its grid has been reconciled by live output and needs no opening hold.
const FRESH_SEED_MAX_AGE: Duration = Duration::from_secs(1);
/// How often the grid path re-checks the window's pane count.
const PANE_COUNT_PROBE_INTERVAL: Duration = Duration::from_secs(1);
/// Row patches beyond this fraction of the window are sent as full frames.
const PATCH_MAX_CHANGED_RATIO: f32 = 0.5;
/// Upper bound on the capture window.
const MAX_WINDOW_LINES: usize = 4000;
/// Floor for the capture window when the client hasn't sized yet.
const DEFAULT_WINDOW_LINES: usize = 50;
/// Keepalive ping interval; the recv side relies on the browser's pong.
const PING_INTERVAL: Duration = Duration::from_secs(30);
/// Floor between drift re-asserts (see the capture loop).
const REASSERT_MIN_INTERVAL: Duration = Duration::from_secs(2);
/// After a drift target proves unreachable (same geometry didn't move after
/// the last re-assert), wait this long before retrying it once, so a transient
/// tmux failure still recovers without spinning the 2s repaint loop.
const STUCK_REASSERT_RETRY: Duration = Duration::from_secs(30);

/// The owner loop's view of a size drift.
#[derive(Clone, Copy, PartialEq, Eq)]
struct DriftGeometry {
    want_cols: u16,
    want_rows: u16,
    pane_cols: u16,
    pane_rows: u16,
}

/// Suppresses re-asserting a drift target that has proven unreachable.
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

    /// True when this drift geometry should trigger a re-assert.
    fn should_reassert(&mut self, geom: DriftGeometry, now: Instant) -> bool {
        match self.last {
            Some((last, at)) if last == geom && now.duration_since(at) < self.retry_after => false,
            _ => {
                self.last = Some((geom, now));
                true
            }
        }
    }

    /// Forget the last target so the next drift re-asserts immediately.
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
    /// Request the lock when it is vacant, without resizing or displacing a live owner.
    #[serde(rename = "claim_if_vacant")]
    ClaimIfVacant,
    /// Explicit "take over" from a non-owner client.
    #[serde(rename = "claim")]
    Claim,
    /// Capability advertisement; see the module doc.
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

/// Which transport renders a live surface.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum LiveTransport {
    Grid,
    Snapshot,
}

/// Shared per-connection knobs the recv loop writes and the capture loop reads.
struct LiveSettings {
    window_lines: AtomicUsize,
    fast: AtomicBool,
    /// Grid from the latest client resize.
    screen_rows: AtomicU64,
    screen_cols: AtomicU64,
    /// True while this connection holds the cross-process size-owner lock.
    is_owner: AtomicBool,
    /// Client advertised `caps.deflate`: frames go out as the compressed
    /// binary stream instead of JSON text. Set-once (a client never revokes).
    deflate: AtomicBool,
    /// Client advertised `caps.patch`: publish row patches when few rows changed.
    patch: AtomicBool,
    /// The next publish must be a full frame (client resync).
    force_full: AtomicBool,
    /// [`live_now_ms`] until which frames at a pane geometry other than the
    /// requested grid are withheld after an owner resize; 0 when none.
    resize_settle_until_ms: AtomicU64,
}

impl LiveSettings {
    fn new() -> Self {
        Self {
            window_lines: AtomicUsize::new(DEFAULT_WINDOW_LINES),
            fast: AtomicBool::new(true),
            screen_rows: AtomicU64::new(0),
            screen_cols: AtomicU64::new(0),
            is_owner: AtomicBool::new(false),
            deflate: AtomicBool::new(false),
            patch: AtomicBool::new(false),
            force_full: AtomicBool::new(false),
            resize_settle_until_ms: AtomicU64::new(0),
        }
    }

    /// Withhold frames still at the old geometry after a resize this connection drove as
    /// size owner, so the client sees one clean repaint.
    fn record_owner_resize(&self, owned: bool) {
        if let Some(settle_until_ms) = resize_follow_up(owned, live_now_ms()) {
            self.resize_settle_until_ms
                .store(settle_until_ms, Ordering::Relaxed);
        }
    }
}

static LIVE_CLOCK: std::sync::LazyLock<Instant> = std::sync::LazyLock::new(Instant::now);

fn live_now_ms() -> u64 {
    LIVE_CLOCK.elapsed().as_millis() as u64
}

/// Whether a frame must be withheld during the post-resize settle window.
fn resize_settle_holds(now_ms: u64, until_ms: u64, want: (u16, u16), have: (u16, u16)) -> bool {
    now_ms < until_ms && want != have
}

/// When old-geometry frames stop being withheld after a resize this connection
/// drove, or `None` when it did not own the resize and nothing moved.
fn resize_follow_up(owned: bool, now_ms: u64) -> Option<u64> {
    owned.then(|| now_ms + RESIZE_SETTLE_MS)
}

/// Resize the pane as size owner and rebuild the VT grid to match.
#[cfg(unix)]
fn resize_and_reseed(
    session: &crate::tmux::Session,
    who: &str,
    ch: Option<&crate::tmux::vt::VtChannel>,
    cols: u16,
    rows: u16,
) -> bool {
    let in_flight = ch.map(|ch| ch.begin_resize(cols, rows));
    let owned = session.resize_window_if_owner(who, cols, rows);
    match (owned, ch, in_flight) {
        (true, Some(ch), _guard) => {
            let deadline = crate::tmux::TmuxCommandDeadline::new();
            ch.set_grid_size_with_deadline(cols, rows, &deadline);
        }
        (false, _, Some(guard)) => guard.abandon(),
        _ => {}
    }
    owned
}

#[cfg(not(unix))]
fn resize_and_reseed(session: &crate::tmux::Session, who: &str, cols: u16, rows: u16) -> bool {
    session.resize_window_if_owner(who, cols, rows)
}

/// The VT grid renders a single-pane window within its scrollback depth; a
/// split window is composited from `capture-pane`.
#[cfg(unix)]
fn grid_transport_eligible(pane_count: Option<u16>, window_lines: usize) -> bool {
    pane_count == Some(1) && window_lines <= crate::tmux::vt::SCROLLBACK_LINES
}

/// Resolve a pending resize expectation while the grid is out of service.
#[cfg(unix)]
fn retry_pending_resync(
    name: &str,
    who: &str,
    is_owner: bool,
    ch: Option<&crate::tmux::vt::VtChannel>,
    deadline: &crate::tmux::TmuxCommandDeadline,
) {
    let Some(ch) = ch else {
        return;
    };
    if ch.pending_resync_target().is_none() {
        return;
    }
    ch.reconcile_with_deadline(deadline);
    let Some((cols, rows)) = ch.pending_resync_target() else {
        return;
    };
    if !is_owner || !crate::tmux::Session::from_name(name).refresh_size_owner(who) {
        return;
    }
    ch.set_grid_size_with_deadline(cols, rows, deadline);
}

/// Rewrite bare cursor-key sequences for an app in DECCKM (application cursor) mode.
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

/// Bytes the pane should receive for `raw` browser input.
#[cfg(unix)]
fn pane_input_bytes(tmux_name: &str, raw: Vec<u8>) -> Vec<u8> {
    match crate::tmux::vt::cursor_mode(tmux_name) {
        Some(app_cursor) => translate_cursor_keys(&raw, app_cursor).into_owned(),
        None => raw,
    }
}

/// Split a frame's content into rows.
fn frame_lines(content: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = content.split('\n').collect();
    if lines.len() > 1 && lines.last() == Some(&"") {
        lines.pop();
    }
    lines
}

/// Rows of `next` that differ from `prev` once `prev` is slid up by `shift` rows (history
/// grew by `shift` lines, so row `i` of the new window was row `i + shift` of the old).
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

/// Which transport is producing frames.
fn transport_json(grid: bool) -> String {
    serde_json::json!({ "type": "transport", "grid": grid }).to_string()
}

/// Whether this connection may push the pane's OSC 52 copies into the viewer's browser
/// clipboard.
#[cfg(unix)]
fn clipboard_forward_enabled(
    mode: crate::session::config::TmuxSettingMode,
    read_only: bool,
) -> bool {
    !read_only && mode != crate::session::config::TmuxSettingMode::Disabled
}

/// Connection-lifetime deflate stream for frame messages (module doc, `caps`).
struct FrameDeflater {
    stream: flate2::Compress,
    input: Vec<u8>,
}

impl FrameDeflater {
    fn new() -> Self {
        Self {
            // Raw deflate, no zlib wrapper.
            stream: flate2::Compress::new(flate2::Compression::fast(), false),
            input: Vec::new(),
        }
    }

    /// Compress one frame into one binary WS payload.
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
    /// A renderable frame.
    Frame(String, Option<crate::tmux::PaneCursor>),
    /// The pane looks gone (dead channel, or an empty capture).
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
#[derive(Deserialize, Default)]
pub struct TerminalIndexQuery {
    #[serde(default)]
    pub index: u32,
}

/// Live view for the paired host shell (TerminalSession).
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
    socket: WebSocket,
    tmux_name: String,
    read_only: bool,
    shutdown: tokio_util::sync::CancellationToken,
    transport: LiveTransport,
) {
    handle_live_ws_inner(
        socket,
        tmux_name,
        read_only,
        shutdown,
        transport,
        #[cfg(test)]
        false,
    )
    .await;
}

#[cfg(test)]
struct CaptureCycleWitness(Option<tokio::sync::mpsc::OwnedPermit<Message>>, bool, bool);

#[cfg(test)]
impl Drop for CaptureCycleWitness {
    fn drop(&mut self) {
        if let Some(permit) = self.0.take() {
            permit.send(Message::Text(
                serde_json::json!({"type": "test_cycle", "grid": self.1, "settled_seed": self.2})
                    .to_string()
                    .into(),
            ));
        }
    }
}

async fn handle_live_ws_inner(
    mut socket: WebSocket,
    tmux_name: String,
    read_only: bool,
    shutdown: tokio_util::sync::CancellationToken,
    transport: LiveTransport,
    #[cfg(test)] witness_cycles: bool,
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

    let settings = Arc::new(LiveSettings::new());
    // Identifies this connection in the cross-process size-owner lock (shared
    // with the web PTY attach and the native TUI via tmux user options).
    let owner_id = format!(
        "live-{}",
        LIVE_CLIENT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    // Wakes the capture loop out of its inter-capture sleep.
    let nudge = Arc::new(tokio::sync::Notify::new());

    #[cfg(unix)]
    let config = crate::session::config::Config::load_or_warn();
    #[cfg(unix)]
    let clipboard_forward = clipboard_forward_enabled(config.tmux.clipboard, read_only);
    // The agent surface renders from the shared VT grid when one arms (the native TUI
    // preview shares it).
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
    // Snapshot surfaces keep OSC 52 through a raw observer that builds no grid.
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

    // Capture loop.
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
        // This connection's own change receiver.
        #[cfg(unix)]
        let mut vt_rx = capture_vt.as_ref().map(|ch| ch.subscribe());
        // Pane count of the window, re-probed at most once per PANE_COUNT_PROBE_INTERVAL
        // while the grid path is in use.
        #[cfg(unix)]
        let mut pane_count: (Option<u16>, Instant) =
            (None, Instant::now() - PANE_COUNT_PROBE_INTERVAL);
        #[cfg(unix)]
        let mut first_publish_wait_started: Option<Instant> = None;
        let mut last_published: Option<(String, Option<crate::tmux::PaneCursor>)> = None;
        // Announced on the first frame and whenever it flips, so a client can
        // report the transport rather than infer it.
        let mut announced_grid: Option<bool> = None;
        // Patch baseline.
        let mut last_sent: Option<(Vec<String>, u32)> = None;
        let mut seq: u64 = 0;
        let mut stats = LiveStats::default();
        // Created on the first frame after the client advertises deflate;
        // lives for the connection so the dictionary spans frames.
        let mut deflater: Option<FrameDeflater> = None;
        let mut dead_probes: u32 = 0;
        let mut last_reassert = std::time::Instant::now() - REASSERT_MIN_INTERVAL;
        #[cfg(unix)]
        let mut last_grid_resync = Instant::now() - GRID_RESYNC_RETRY;
        let mut reassert_guard = ReassertGuard::new(STUCK_REASSERT_RETRY);
        let mut last_heartbeat = std::time::Instant::now() - SIZE_OWNER_HEARTBEAT;
        let mut last_reclaim = std::time::Instant::now() - SIZE_OWNER_HEARTBEAT;
        loop {
            // The grid serves single-pane windows within its scrollback depth;
            // a split window is composited from capture-pane.
            #[cfg(unix)]
            let live_grid = capture_vt.as_ref().filter(|ch| ch.is_alive()).cloned();
            // A resize whose reseed did not land left the parser at the old geometry.
            #[cfg(unix)]
            if last_grid_resync.elapsed() >= GRID_RESYNC_RETRY
                && live_grid
                    .as_ref()
                    .is_some_and(|ch| ch.grid_resync_pending())
            {
                let name = capture_tmux.clone();
                let who = capture_owner.clone();
                let is_owner = capture_settings.is_owner.load(Ordering::Relaxed);
                let ch = live_grid.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    let deadline = crate::tmux::TmuxCommandDeadline::new();
                    retry_pending_resync(&name, &who, is_owner, ch.as_deref(), &deadline);
                })
                .await;
                // Throttle from the end of the attempt.
                last_grid_resync = Instant::now();
            }

            let sample_started = std::time::Instant::now();
            let lines = capture_settings.window_lines.load(Ordering::Relaxed);

            // Fetch tmux's authoritative rendered cells.
            let outcome: CaptureOutcome;
            #[cfg(unix)]
            let mut grid_frame = false;
            // Set from the sample itself, not from a later hold check.
            #[cfg(unix)]
            let mut grid_incomplete = false;
            #[cfg(unix)]
            {
                if live_grid.is_some() && pane_count.1.elapsed() >= PANE_COUNT_PROBE_INTERVAL {
                    let name = capture_tmux.clone();
                    // Advance the probe clock even on failure, or a tmux that cannot answer
                    // would be re-forked on every capture cycle instead of once a second.
                    let probed =
                        tokio::task::spawn_blocking(move || window_pane_count(&name)).await;
                    pane_count = (probed.ok().flatten().or(pane_count.0), Instant::now());
                }
                outcome = match live_grid {
                    Some(ch) if grid_transport_eligible(pane_count.0, lines) => {
                        grid_frame = true;
                        match tokio::task::spawn_blocking(move || {
                            let deadline = crate::tmux::TmuxCommandDeadline::new();
                            ch.sample_with_deadline(lines, &deadline)
                        })
                        .await
                        {
                            Ok(sample) => {
                                grid_incomplete = sample.incomplete;
                                CaptureOutcome::Frame(sample.content, sample.cursor)
                            }
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

            #[cfg(test)]
            let _cycle_witness = CaptureCycleWitness(
                if witness_cycles {
                    capture_tx.clone().reserve_owned().await.ok()
                } else {
                    None
                },
                {
                    #[cfg(unix)]
                    {
                        grid_frame
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                },
                {
                    #[cfg(unix)]
                    {
                        capture_vt
                            .as_ref()
                            .is_some_and(|ch| ch.seed_age() >= FRESH_SEED_MAX_AGE)
                    }
                    #[cfg(not(unix))]
                    {
                        false
                    }
                },
            );
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
                    // Auto-reclaim.
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
                            #[cfg(unix)]
                            let reclaim_vt = capture_vt.clone();
                            let claimed = tokio::task::spawn_blocking(move || {
                                let session = crate::tmux::Session::from_name(&name);
                                if !session.claim_size_owner(&who, SIZE_OWNER_TTL) {
                                    return false;
                                }
                                #[cfg(unix)]
                                let owned = resize_and_reseed(
                                    &session,
                                    &who,
                                    reclaim_vt.as_deref(),
                                    cols,
                                    rows,
                                );
                                #[cfg(not(unix))]
                                let owned = resize_and_reseed(&session, &who, cols, rows);
                                owned
                            })
                            .await
                            .unwrap_or(false);
                            // This resize moves the pane like any other the
                            // owner drives, so it owes the same settle window.
                            capture_settings.record_owner_resize(claimed);
                            if claimed {
                                capture_settings.is_owner.store(true, Ordering::Relaxed);
                                last_heartbeat = std::time::Instant::now();
                                let _ = capture_tx
                                    .send(Message::Text(size_owner_json(true).into()))
                                    .await;
                            }
                        }
                    }
                    // Only the owner drives the window size.
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
                            // Re-assert only for a genuine, not-yet-proven-stuck drift.
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
                                // Verified resize.
                                let name = capture_tmux.clone();
                                let who = capture_owner.clone();
                                #[cfg(unix)]
                                let reassert_vt = capture_vt.clone();
                                let still_owner = tokio::task::spawn_blocking(move || {
                                    let session = crate::tmux::Session::from_name(&name);
                                    #[cfg(unix)]
                                    let owned = resize_and_reseed(
                                        &session,
                                        &who,
                                        reassert_vt.as_deref(),
                                        want_cols,
                                        want_rows,
                                    );
                                    #[cfg(not(unix))]
                                    let owned =
                                        resize_and_reseed(&session, &who, want_cols, want_rows);
                                    owned
                                })
                                .await
                                .unwrap_or(false);
                                capture_settings.record_owner_resize(still_owner);
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
                    // Post-resize settle.
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
                    // Mid-bracket grid (the app is inside a synchronized-output repaint, or
                    // a reseed just copied tmux's half-drawn cells).
                    #[cfg(unix)]
                    if grid_frame
                        && capture_vt
                            .as_ref()
                            .is_some_and(|ch| ch.grid_resync_pending())
                    {
                        stats.settle_held += 1;
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
                    #[cfg(unix)]
                    if grid_frame
                        && (grid_incomplete
                            || capture_vt.as_ref().is_some_and(|ch| ch.sync_hold_active()))
                    {
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
                    // First publish from a freshly seeded grid.
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
                        // The two transports serialize the same screen from different
                        // sources, and carry their own scrollback depth with it.
                        last_sent = None;
                        if capture_tx
                            .send(Message::Text(transport_json(grid_frame).into()))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    let frame = (content, cursor);
                    // A resync republishes even when the frame is unchanged.
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
                                    // Corrupt compressor state (not expected).
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
                        // A live VT channel with socket input (ours or another surface's)
                        // is the pane's single input writer; otherwise input goes through
                        // tmux send-keys.
                        let _ = tokio::task::spawn_blocking(move || {
                            #[cfg(unix)]
                            let bytes = pane_input_bytes(&name, bytes);
                            #[cfg(unix)]
                            if crate::tmux::vt::input_mode(&name).is_some()
                                && crate::tmux::vt::try_send_input(&name, &bytes)
                            {
                                return;
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
                                // Claim the cross-process size-owner lock; only the owner
                                // resizes the shared window.
                                let name = tmux_name.clone();
                                let who = owner_id.clone();
                                #[cfg(unix)]
                                let resize_vt = vt.clone();
                                let owned = tokio::task::spawn_blocking(move || {
                                    let session = crate::tmux::Session::from_name(&name);
                                    if !session.claim_size_owner(&who, SIZE_OWNER_TTL) {
                                        return false;
                                    }
                                    #[cfg(unix)]
                                    let owned = resize_and_reseed(
                                        &session,
                                        &who,
                                        resize_vt.as_deref(),
                                        cols,
                                        rows,
                                    );
                                    #[cfg(not(unix))]
                                    let owned = resize_and_reseed(&session, &who, cols, rows);
                                    owned
                                })
                                .await
                                .unwrap_or(false);
                                settings.record_owner_resize(owned);
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
                                // A keyboard-open mobile pane intentionally postpones its
                                // first resize so it never sends keyboard-shrunk rows to
                                // tmux.
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
                                // Explicit take-over.
                                let name = tmux_name.clone();
                                let who = owner_id.clone();
                                let cols = settings.screen_cols.load(Ordering::Relaxed) as u16;
                                let rows = settings.screen_rows.load(Ordering::Relaxed) as u16;
                                #[cfg(unix)]
                                let claim_vt = vt.clone();
                                let (owned, resized) = tokio::task::spawn_blocking(move || {
                                    let session = crate::tmux::Session::from_name(&name);
                                    if !session.steal_size_owner(&who) {
                                        return (false, false);
                                    }
                                    if cols == 0 || rows == 0 {
                                        return (true, false);
                                    }
                                    #[cfg(unix)]
                                    let owned = resize_and_reseed(
                                        &session,
                                        &who,
                                        claim_vt.as_deref(),
                                        cols,
                                        rows,
                                    );
                                    #[cfg(not(unix))]
                                    let owned = resize_and_reseed(&session, &who, cols, rows);
                                    (owned, owned)
                                })
                                .await
                                .unwrap_or((false, false));
                                settings.record_owner_resize(resized);
                                settings.is_owner.store(owned, Ordering::Relaxed);
                                let _ = out_tx
                                    .send(Message::Text(size_owner_json(owned).into()))
                                    .await;
                                nudge.notify_one();
                            }
                            LiveControlMessage::Caps { deflate, patch } => {
                                // Set-once.
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

    // Release the size-owner lock if we held it.
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

/// Serialize one snapshot frame.
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

/// Number of panes in the session's first window, or `None` if tmux could not answer.
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

/// Sleep until the next reason to sample.
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
    // The cursor is pane relative while composited content uses the window grid.
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
    // Full-screen (alternate-screen) mouse apps have no capturable scrollback; the client
    // forwards the wheel to the app instead of widening the capture window.
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

    /// #2766: a drift target that did not move is re-asserted once, then suppressed
    /// until the retry window, so a transient tmux failure still recovers without
    /// spinning the repaint loop. A genuinely new target, or a reset after the pane
    /// reached its size, fires immediately.
    #[test]
    fn reassert_guard_suppresses_only_an_unchanged_stuck_target() {
        let mut g = ReassertGuard::new(STUCK_REASSERT_RETRY);
        let stuck = geom((115, 67), (115, 66));
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);

        assert!(g.should_reassert(stuck, t0), "first drift");
        assert!(!g.should_reassert(stuck, at(2)), "identical target");
        assert!(!g.should_reassert(stuck, at(20)), "still inside the window");
        assert!(
            g.should_reassert(stuck, t0 + STUCK_REASSERT_RETRY + Duration::from_secs(1)),
            "retried once past the window"
        );
        // Without the reset, t0+35s sits inside the window opened by that retry.
        g.reset();
        assert!(g.should_reassert(stuck, at(35)), "reset clears the window");

        let mut g = ReassertGuard::new(STUCK_REASSERT_RETRY);
        assert!(g.should_reassert(stuck, t0));
        assert!(
            g.should_reassert(geom((120, 70), (115, 66)), at(1)),
            "a real resize is a different target"
        );
    }

    fn cursor() -> crate::tmux::PaneCursor {
        crate::tmux::PaneCursor {
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
        }
    }

    fn frame_value(cursor: Option<&crate::tmux::PaneCursor>) -> serde_json::Value {
        serde_json::from_str(&frame_json("hello\nworld", cursor, 1)).unwrap()
    }

    #[test]
    fn frame_json_includes_geometry_and_cursor() {
        // (pane 0 of a composited split, the cursor in window coordinates, `pane0`)
        let cases = [
            // Unsplit: `pane0` is null and the cursor needs no offset.
            (None, (3, 7), serde_json::Value::Null),
            // Composited with pane 0 at the corner (a borderless split).
            (
                Some(crate::tmux::PaneGeom {
                    left: 0,
                    top: 0,
                    width: 37,
                    height: 46,
                }),
                (3, 7),
                serde_json::json!({"cols": 37, "rows": 46, "left": 0, "top": 0}),
            ),
            // Composited with pane-border-status top: the pane-relative cursor is
            // shifted onto the window grid the content is composited into.
            (
                Some(crate::tmux::PaneGeom {
                    left: 2,
                    top: 1,
                    width: 37,
                    height: 46,
                }),
                (5, 8),
                serde_json::json!({"cols": 37, "rows": 46, "left": 2, "top": 1}),
            ),
        ];
        for (pane0, want_cursor, want_pane0) in cases {
            let mut c = cursor();
            c.composite_pane0 = pane0;
            let v = frame_value(Some(&c));
            assert_eq!(v["type"], "frame");
            assert_eq!(v["content"], "hello\nworld");
            assert_eq!(v["rows"], 46);
            assert_eq!(v["history"], 1200);
            assert_eq!(v["cursor"]["x"], want_cursor.0, "{want_pane0}");
            assert_eq!(v["cursor"]["y"], want_cursor.1, "{want_pane0}");
            assert_eq!(v["altScreen"], false);
            assert_eq!(v["mouse"], false);
            assert_eq!(v["mouseSgr"], false);
            assert_eq!(v["pane0"], want_pane0);
        }
    }

    #[test]
    fn frame_json_reports_cursor_modes() {
        let mut alt = cursor();
        alt.alternate_on = true;
        alt.mouse_tracking = true;
        let v = frame_value(Some(&alt));
        assert_eq!(v["altScreen"], true);
        assert_eq!(v["mouse"], true);
        assert_eq!(v["mouseSgr"], false);

        // DECTCEM off hides the cursor without losing the geometry.
        let mut hidden = cursor();
        hidden.visible = false;
        let v = frame_value(Some(&hidden));
        assert!(v["cursor"].is_null());
        assert_eq!(v["rows"], 46);

        // No cursor at all: nothing knows the pane height either.
        let v = frame_value(None);
        assert!(v["cursor"].is_null());
        assert_eq!(v["rows"], 0);
    }

    #[test]
    fn control_messages_parse() {
        fn parse(json: &str) -> LiveControlMessage {
            serde_json::from_str(json).unwrap()
        }
        use LiveControlMessage::*;
        assert!(matches!(
            parse(r#"{"type":"resize","cols":74,"rows":46}"#),
            Resize { cols: 74, rows: 46 }
        ));
        assert!(matches!(
            parse(r#"{"type":"window","lines":800}"#),
            Window { lines: 800 }
        ));
        assert!(matches!(
            parse(r#"{"type":"cadence","fast":false}"#),
            Cadence { fast: false }
        ));
        assert!(matches!(parse(r#"{"type":"claim"}"#), Claim));
        assert!(matches!(
            parse(r#"{"type":"claim_if_vacant"}"#),
            ClaimIfVacant
        ));
        // `patch` defaults to false when the client does not advertise it.
        assert!(matches!(
            parse(r#"{"type":"caps","deflate":true}"#),
            Caps {
                deflate: true,
                patch: false
            }
        ));
        assert!(matches!(
            parse(r#"{"type":"caps","deflate":true,"patch":true}"#),
            Caps {
                deflate: true,
                patch: true
            }
        ));
        assert!(matches!(parse(r#"{"type":"resync"}"#), Resync));
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
        // Frame 2.
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
        // The cross-frame dictionary is the point.
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
            // History grew by one.
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

    #[cfg(unix)]
    #[test]
    fn pane_input_bytes_translates_cursor_keys_for_an_output_only_live_grid() {
        // tmux < 3.8 arms output-only channels, so input takes `send-keys -H`, which is as
        // literal as the socket.
        let name = format!("aoe_test_ws_cursor_{}", std::process::id());
        let dir = tempfile::tempdir().expect("tempdir");
        let _channel = crate::tmux::vt::register_live_for_test(&name, dir.path(), false, true);
        assert_eq!(pane_input_bytes(&name, b"\x1b[A".to_vec()), b"\x1bOA");
        crate::tmux::vt::unregister_for_test(&name);
        // No live grid: nothing knows the mode, bytes pass through.
        assert_eq!(pane_input_bytes(&name, b"\x1b[A".to_vec()), b"\x1b[A");
    }

    #[test]
    fn resize_settle_holds_only_mismatched_geometry_inside_the_window() {
        assert!(resize_settle_holds(100, 400, (80, 24), (120, 40)));
        assert!(!resize_settle_holds(100, 400, (80, 24), (80, 24)));
        assert!(!resize_settle_holds(500, 400, (80, 24), (120, 40)));
    }

    #[test]
    fn resize_follow_up_arms_only_an_owned_resize() {
        assert_eq!(resize_follow_up(true, 100), Some(100 + RESIZE_SETTLE_MS));
        assert_eq!(resize_follow_up(false, 100), None);
        let settings = LiveSettings::new();
        settings.record_owner_resize(true);
        let settle_until = settings.resize_settle_until_ms.load(Ordering::Relaxed);
        assert!(settle_until > 0);
        settings.record_owner_resize(false);
        assert_eq!(
            settings.resize_settle_until_ms.load(Ordering::Relaxed),
            settle_until
        );
    }

    #[cfg(unix)]
    #[test]
    #[serial_test::serial]
    fn a_resize_whose_reseed_missed_withholds_frames_until_it_lands() {
        use futures_util::FutureExt;

        let home = crate::session::test_support::isolate_app_dir();
        let _socket = crate::session::test_support::EnvGuard::set(&[(
            "AOE_TMUX_SOCKET",
            home.path().join("tmux.sock"),
        )]);
        if crate::tmux::tmux_command().arg("-V").output().is_err() {
            eprintln!("Skipping test: tmux unavailable");
            return;
        }
        let pane = crate::tmux::test_helpers::TmuxTestSession::new("aoe_test_ws_busy");
        let output = crate::tmux::tmux_command()
            .args([
                "new-session",
                "-d",
                "-s",
                pane.name(),
                "-x",
                "40",
                "-y",
                "6",
                "printf 'RESYNC-LANDED'; exec cat",
            ])
            .output()
            .expect("create resized pane");
        assert!(output.status.success());
        let target = crate::tmux::test_helpers::only_pane_id(pane.name());
        crate::tmux::test_helpers::wait_for_pane_command(&target, "cat");
        let native_dir = tempfile::tempdir().unwrap();
        let channel =
            crate::tmux::vt::register_live_for_test(pane.name(), native_dir.path(), false, false);
        let mut held = channel.hold_drain_for_test();
        let result =
            channel.set_grid_size_with_deadline(40, 6, &crate::tmux::TmuxCommandDeadline::new());
        assert_eq!(result, crate::tmux::vt::VtRefreshResult::Busy);
        assert!(
            held.observed_probe(),
            "native drain exercised while ACK withheld"
        );
        assert!(channel.grid_resync_pending());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            // One capture cycle may use three native command budgets.
            let progress_timeout = crate::tmux::TMUX_COMMAND_TIMEOUT * 3;
            let (closed_tx, mut closed_rx) = tokio::sync::mpsc::unbounded_channel();
            let shutdown = tokio_util::sync::CancellationToken::new();
            let route_shutdown = shutdown.clone();
            let name = pane.name().to_string();
            let router = axum::Router::new().route(
                "/live",
                axum::routing::get(move |ws: WebSocketUpgrade| {
                    let name = name.clone();
                    let shutdown = route_shutdown.clone();
                    let closed_tx = closed_tx.clone();
                    async move {
                        ws.on_upgrade(move |socket| async move {
                            handle_live_ws_inner(
                                socket,
                                name,
                                true,
                                shutdown,
                                LiveTransport::Grid,
                                true,
                            )
                            .await;
                            let _ = closed_tx.send(());
                        })
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server_shutdown = shutdown.clone();
            let server = tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(server_shutdown.cancelled_owned())
                    .await
                    .unwrap();
            });
            let (mut client, _) = tokio_tungstenite::connect_async(format!("ws://{address}/live"))
                .await
                .unwrap();
            let checked = std::panic::AssertUnwindSafe(async {
                let withheld = tokio::time::timeout(progress_timeout, async {
                    loop {
                        let message = client
                            .next()
                            .await
                            .expect("open websocket")
                            .expect("websocket message");
                        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                            assert_ne!(
                                value["type"], "frame",
                                "stale grid escaped before its native resync"
                            );
                            assert_ne!(value["type"], "patch");
                            if value["type"] == "test_cycle" {
                                assert_eq!(
                                    value["grid"], true,
                                    "must exercise the grid publication path"
                                );
                                if value["settled_seed"] == true {
                                    break;
                                }
                            }
                        }
                    }
                })
                .await;
                withheld.expect("completed withheld grid cycles");
                assert!(channel.grid_resync_pending());
                held.acknowledge_next();
                let landed = tokio::time::timeout(progress_timeout, async {
                    loop {
                        let message = client
                            .next()
                            .await
                            .expect("open websocket")
                            .expect("websocket message");
                        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
                            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                            if value["type"] == "frame" {
                                assert!(value["content"]
                                    .as_str()
                                    .unwrap()
                                    .contains("RESYNC-LANDED"));
                                assert_eq!(value["rows"], 6);
                                break;
                            }
                        }
                    }
                })
                .await;
                landed.expect("frame publishes after native ACK and real pane reseed");
                assert!(!channel.grid_resync_pending());
            })
            .catch_unwind()
            .await;
            let close = tokio::time::timeout(progress_timeout, client.close(None)).await;
            shutdown.cancel();
            let closed = tokio::time::timeout(progress_timeout, closed_rx.recv()).await;
            let served = server.await;
            if let Err(panic) = checked {
                std::panic::resume_unwind(panic);
            }
            close
                .expect("websocket close completes")
                .expect("close websocket");
            closed
                .expect("upgraded handler exits")
                .expect("handler completion witness");
            served.expect("HTTP server exits");
        });
        drop(held);
        crate::tmux::vt::unregister_for_test(pane.name());
    }

    #[test]
    fn grid_transport_needs_a_single_pane_within_the_grids_scrollback() {
        assert!(grid_transport_eligible(Some(1), 50));
        // Unprobed or split windows are composited from capture-pane.
        assert!(!grid_transport_eligible(None, 50));
        assert!(!grid_transport_eligible(Some(2), 50));
        // A window deeper than the grid keeps that history.
        assert!(!grid_transport_eligible(
            Some(1),
            crate::tmux::vt::SCROLLBACK_LINES + 1
        ));
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
