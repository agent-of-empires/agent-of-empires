//! WebSocket handler for live terminal streaming via PTY relay.
//!
//! Instead of polling `capture-pane`, each WebSocket connection spawns
//! `tmux attach-session` inside a PTY and relays the raw byte stream
//! bidirectionally. This gives the browser a real terminal experience:
//! zero input lag, all key sequences work, real-time output.

use std::io::{Read, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{
    extract::{
        ws::{CloseFrame, Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    response::IntoResponse,
};

/// Close code we send when the PTY relay exited with the underlying
/// pane gone (`tmux attach-session` failed, tmux session was destroyed,
/// PTY EOF before any byte reached the browser). The web terminal hook
/// treats this as "stop retrying immediately, surface the manual
/// reconnect banner" rather than burning the retry budget against a
/// permanently broken pane. Picked from the application-reserved
/// 4000-4999 range; not used elsewhere. See #1107.
const CLOSE_CODE_PTY_DEAD: u16 = 4001;

/// WebSocket close code 1001 ("going away"). Sent when the daemon is
/// shutting down so the client can distinguish a server-side exit from
/// a transient transport error and skip its reconnect backoff for one
/// cycle. See #1198.
const CLOSE_CODE_GOING_AWAY: u16 = 1001;

/// WebSocket close code 1011 ("internal server error"). Sent on the
/// OS-level early-return paths in `handle_terminal_ws` (openpty,
/// attach-session spawn, PTY reader/writer clone). Browser falls through
/// to its standard retry ladder with a parseable reason. Beats the
/// opaque 1006 that early `return;` would otherwise produce. See #1455.
const CLOSE_CODE_INTERNAL_ERROR: u16 = 1011;

/// WebSocket close code 1013 ("try again later"). Sent when the tmux
/// pane is not ready within the bounded readiness window. Browser
/// retries on the fast-start ladder. Distinct from 1011 (internal
/// server fault) and 4001 (permanently dead pane) so logs separate
/// transient warm-up from genuine failure. See #1455.
const CLOSE_CODE_TRY_AGAIN_LATER: u16 = 1013;

/// Total time we'll spend waiting for the tmux session + pane to be
/// attachable before giving up and closing 1013. 2s covers tmux warm-up
/// across the slow machines we've seen reports from while staying short
/// enough that a truly dead pane doesn't hold the upgrade open for the
/// user. See #1455.
const TMUX_READY_TIMEOUT: Duration = Duration::from_millis(2000);

/// Poll interval for the readiness wait. 50ms gives ~40 probes inside
/// the 2s window; each probe shells out to `tmux has-session` and (if
/// that passes) `tmux list-panes`, which is cheap.
const TMUX_READY_POLL: Duration = Duration::from_millis(50);
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, trace, warn};

/// Per-message byte tracing is hidden behind `AOE_TERMINAL_TRACE=1` so the
/// default `AOE_LOG_LEVEL=debug` run captures lifecycle without drowning
/// the log in PTY-byte chatter (a busy claude session emits thousands of
/// frames/min). Read once at connect time so the gate is a single atomic
/// load per message instead of an env lookup.
fn terminal_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var("AOE_TERMINAL_TRACE").is_ok())
}

use super::AppState;

/// WebSocket for the paired host terminal (TerminalSession tmux session)
pub async fn paired_terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    debug!(target: "terminal.ws", session = %id, kind = "paired", "ws route entered");
    let instances = state.instances.read().await;
    let inst = instances.iter().find(|i| i.id == id).cloned();
    drop(instances);

    let read_only = state.read_only;
    let primaries = Arc::clone(&state.session_primaries);
    let pause_counts = Arc::clone(&state.session_pause_counts);
    let shutdown = state.shutdown.clone();

    let Some(inst) = inst else {
        warn!(target: "terminal.ws", session = %id, kind = "paired", "session not found, returning 404");
        return (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response();
    };

    // Auto-respawn a dead pane before upgrading. The browser's WS reconnect
    // path goes straight to this route without re-running ensure_terminal,
    // so without this check a pane that died while the page stayed open
    // (most commonly across an `aoe serve` restart) would attach to a
    // tombstone that swallows every keystroke. Match the kill+recreate
    // dance in `ensure_terminal` and the TUI attach path.
    let tmux_name = match respawn_paired_if_dead(&state, &id, &inst).await {
        Ok(name) => name,
        Err(e) => {
            warn!(
                target: "terminal.ws",
                session = %id,
                kind = "paired",
                "failed to respawn dead pane: {}", e
            );
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to revive terminal",
            )
                .into_response();
        }
    };

    // Accept the "aoe-auth" subprotocol so the browser's handshake
    // completes. The client offers `["aoe-auth", <token>]`; the auth
    // middleware validates the token from the same header, and the
    // server echoes back "aoe-auth" to satisfy the WS spec. The token
    // itself is not echoed, only the marker.
    ws.protocols(["aoe-auth"])
        .on_upgrade(move |socket| {
            handle_terminal_ws(
                socket,
                tmux_name,
                read_only,
                primaries,
                pause_counts,
                shutdown,
            )
        })
        .into_response()
}

/// Returns the tmux session name to attach to. If the existing pane is
/// dead, kills and recreates the tmux session in a blocking task before
/// returning. The instance's `terminal_info.created` flag is updated in
/// the in-memory store on successful recreate.
async fn respawn_paired_if_dead(
    state: &Arc<AppState>,
    id: &str,
    inst: &crate::session::Instance,
) -> anyhow::Result<String> {
    let tmux_name = crate::tmux::TerminalSession::generate_name(&inst.id, &inst.title);

    // Serialize concurrent reconnects for the same session so two
    // simultaneous WS attaches don't both try to recreate the pane.
    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;

    let mut inst_for_blocking = inst.clone();
    let tmux_name_clone = tmux_name.clone();
    // Two failure modes the user can land in:
    //   1. Pane is dead but the tmux session still exists (shell exit
    //      under `remain-on-exit on`). `kill_terminal_if_dead` clears
    //      the tombstone, then we respawn.
    //   2. The whole tmux session is gone (`tmux kill-session`, daemon
    //      reaped on aoe restart, etc). `kill_terminal_if_dead`
    //      returns false here because there's nothing to kill, but the
    //      next `tmux attach-session` will fail with "can't find
    //      session" and the WS will close with code 4001. Recreate
    //      the session in that case too so the retry click recovers
    //      instead of hot-looping. See #1107 follow-up.
    let respawned = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let killed_dead = inst_for_blocking.kill_terminal_if_dead()?;
        let session_missing = !inst_for_blocking.terminal_tmux_session()?.exists();
        if !killed_dead && !session_missing {
            return Ok(false);
        }
        if killed_dead {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "paired terminal pane dead at WS upgrade, killing and respawning"
            );
        } else {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "paired terminal session missing at WS upgrade, recreating"
            );
        }
        inst_for_blocking.start_terminal()?;
        Ok(true)
    })
    .await
    .map_err(|e| anyhow::anyhow!("respawn task panicked: {e}"))??;

    if respawned {
        let mut instances = state.instances.write().await;
        if let Some(stored) = instances.iter_mut().find(|i| i.id == id) {
            stored.terminal_info = Some(crate::session::TerminalInfo { created: true });
        }
    }

    Ok(tmux_name)
}

/// WebSocket for the paired container terminal (ContainerTerminalSession tmux session)
pub async fn container_terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    debug!(target: "terminal.ws", session = %id, kind = "container", "ws route entered");
    let instances = state.instances.read().await;
    let inst = instances.iter().find(|i| i.id == id).cloned();
    drop(instances);

    let read_only = state.read_only;
    let primaries = Arc::clone(&state.session_primaries);
    let pause_counts = Arc::clone(&state.session_pause_counts);
    let shutdown = state.shutdown.clone();

    let Some(inst) = inst else {
        warn!(target: "terminal.ws", session = %id, kind = "container", "session not found, returning 404");
        return (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response();
    };

    // See `paired_terminal_ws` for the dead-pane rescue rationale.
    let tmux_name = match respawn_container_if_dead(&state, &id, &inst).await {
        Ok(name) => name,
        Err(e) => {
            warn!(
                target: "terminal.ws",
                session = %id,
                kind = "container",
                "failed to respawn dead pane: {}", e
            );
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to revive terminal",
            )
                .into_response();
        }
    };

    // Accept the "aoe-auth" subprotocol so the browser's handshake
    // completes. The client offers `["aoe-auth", <token>]`; the auth
    // middleware validates the token from the same header, and the
    // server echoes back "aoe-auth" to satisfy the WS spec. The token
    // itself is not echoed, only the marker.
    ws.protocols(["aoe-auth"])
        .on_upgrade(move |socket| {
            handle_terminal_ws(
                socket,
                tmux_name,
                read_only,
                primaries,
                pause_counts,
                shutdown,
            )
        })
        .into_response()
}

/// Container-terminal counterpart of [`respawn_paired_if_dead`].
async fn respawn_container_if_dead(
    state: &Arc<AppState>,
    id: &str,
    inst: &crate::session::Instance,
) -> anyhow::Result<String> {
    let tmux_name = crate::tmux::ContainerTerminalSession::generate_name(&inst.id, &inst.title);

    let lock = state.instance_lock(id).await;
    let _guard = lock.lock().await;

    let mut inst_for_blocking = inst.clone();
    let tmux_name_clone = tmux_name.clone();
    // No in-memory cache to update for container terminal: `has_container_terminal()`
    // queries tmux directly, so unlike the paired variant we don't need to write
    // back a `terminal_info` flag after a successful respawn.
    //
    // See `respawn_paired_if_dead` for the missing-session branch: a
    // `tmux kill-session` on a paired container terminal also has to
    // recreate from scratch, not just kill-then-respawn the pane.
    let _respawned = tokio::task::spawn_blocking(move || -> anyhow::Result<bool> {
        let killed_dead = inst_for_blocking.kill_container_terminal_if_dead()?;
        let session_missing = !inst_for_blocking
            .container_terminal_tmux_session()?
            .exists();
        if !killed_dead && !session_missing {
            return Ok(false);
        }
        if killed_dead {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "container terminal pane dead at WS upgrade, killing and respawning"
            );
        } else {
            tracing::warn!(
                target: "terminal.ws",
                tmux = %tmux_name_clone,
                "container terminal session missing at WS upgrade, recreating"
            );
        }
        inst_for_blocking.start_container_terminal_with_size(None)?;
        Ok(true)
    })
    .await
    .map_err(|e| anyhow::anyhow!("respawn task panicked: {e}"))??;

    Ok(tmux_name)
}

/// WebSocket for the agent's main tmux session
pub async fn terminal_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    debug!(target: "terminal.ws", session = %id, kind = "agent", "ws route entered");
    // Verify session exists before upgrading
    let instances = state.instances.read().await;
    let session_info = instances
        .iter()
        .find(|i| i.id == id)
        .map(|inst| crate::tmux::Session::generate_name(&inst.id, &inst.title));
    drop(instances);

    let read_only = state.read_only;
    let primaries = Arc::clone(&state.session_primaries);
    let pause_counts = Arc::clone(&state.session_pause_counts);
    let shutdown = state.shutdown.clone();

    match session_info {
        // Accept the "aoe-auth" subprotocol so the browser's handshake
        // completes. The client offers `["aoe-auth", <token>]`; the auth
        // middleware validates the token from the same header, and the
        // server echoes back "aoe-auth" to satisfy the WS spec. The token
        // itself is not echoed, only the marker.
        Some(tmux_name) => ws
            .protocols(["aoe-auth"])
            .on_upgrade(move |socket| {
                handle_terminal_ws(
                    socket,
                    tmux_name,
                    read_only,
                    primaries,
                    pause_counts,
                    shutdown,
                )
            })
            .into_response(),
        None => {
            warn!(target: "terminal.ws", session = %id, kind = "agent", "session not found, returning 404");
            (axum::http::StatusCode::NOT_FOUND, "Session not found").into_response()
        }
    }
}

/// Unique client ID counter for primary-client tracking.
static CLIENT_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Server-originated Ping cadence. Each Ping elicits a Pong from the
/// browser, which the recv loop's idle timeout treats as proof of life.
/// Also keeps NAT mappings warm on mobile carriers that otherwise drop
/// idle TCP flows.
const PING_INTERVAL: Duration = Duration::from_secs(30);

/// Drop a WebSocket that has been completely silent for this long. With
/// PING_INTERVAL of 30s the client should respond with a Pong every 30s,
/// so 90s tolerates two missed round-trips before tearing down. Without
/// this, an idle tmux session whose client has gone dark (mobile app
/// backgrounded, WiFi blip, abrupt close without Close frame) holds its
/// tmux child + PTY pair until tmux next produces output, which can be
/// hours. That leak is what exhausts RLIMIT_NOFILE under modest load.
const IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// Per-tmux-session map tracking which WebSocket client "owns" resizing.
///
/// Only the primary client's resize messages are applied to its PTY. This
/// prevents multiple browser viewports (phone, desktop, tablet) from
/// fighting over the tmux window size. The primary is whichever client
/// most recently sent keyboard input, since aoe is single-user: if
/// you're typing on your phone you want phone-sized output.
///
/// When no client is primary (all have disconnected, or nobody has typed
/// yet), any client's resize is applied so the initial connection still
/// sets up the PTY dimensions from the default 80x24.
type SessionPrimaries = Arc<RwLock<std::collections::HashMap<String, String>>>;

/// Per-session refcount of clients requesting SIGSTOP of the pane's
/// process tree. Only transitions 0↔1 actually signal the process, so
/// concurrent readers can scroll independently without one's
/// `resume_output` un-pausing the other's scrollback.
type SessionPauseCounts = Arc<tokio::sync::Mutex<std::collections::HashMap<String, u32>>>;

async fn handle_terminal_ws(
    mut socket: WebSocket,
    tmux_name: String,
    read_only: bool,
    primaries: SessionPrimaries,
    pause_counts: SessionPauseCounts,
    shutdown: CancellationToken,
) {
    use futures_util::{SinkExt, StreamExt};

    let client_id = format!(
        "ws-{}",
        CLIENT_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let started_at = Instant::now();
    let trace_bytes = terminal_trace_enabled();
    info!(
        target: "terminal.ws",
        client = %client_id,
        tmux = %tmux_name,
        read_only,
        trace_bytes,
        "ws upgrade complete, starting PTY relay"
    );

    // Wait for tmux session + at least one live pane to be attachable
    // before spawning `tmux attach-session`. Closes the race where the
    // browser dialed in fractions of a second after the session was
    // created and the attach raced the tmux daemon's pane bookkeeping.
    // The `terminal_ws` (agent main) route lacks an analog of
    // `respawn_paired_if_dead`, so without this poll a fresh `aoe serve`
    // would let the first attach exit with PTY EOF, the send task would
    // close 4001 (pty_dead), and the client would burn its retry budget
    // on a session that was about to become healthy. See #1455.
    match wait_for_tmux_ready(&tmux_name).await {
        PaneReadiness::Ready => {}
        PaneReadiness::Dead => {
            warn!(
                target: "terminal.ws",
                client = %client_id,
                tmux = %tmux_name,
                "all panes reported dead during readiness wait, closing 4001"
            );
            close_early(&mut socket, CLOSE_CODE_PTY_DEAD, "pty_dead").await;
            return;
        }
        PaneReadiness::NotReady => {
            warn!(
                target: "terminal.ws",
                client = %client_id,
                tmux = %tmux_name,
                timeout_ms = TMUX_READY_TIMEOUT.as_millis() as u64,
                "tmux not ready within bounded wait, closing 1013"
            );
            close_early(&mut socket, CLOSE_CODE_TRY_AGAIN_LATER, "tmux_not_ready").await;
            return;
        }
    }

    // Spawn tmux attach inside a PTY
    let pty_system = NativePtySystem::default();
    let pair = match pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(pair) => pair,
        Err(e) => {
            error!(
                target: "terminal.ws",
                client = %client_id,
                tmux = %tmux_name,
                "openpty failed, aborting ws: {}", e
            );
            close_early(&mut socket, CLOSE_CODE_INTERNAL_ERROR, "openpty_failed").await;
            return;
        }
    };

    let mut cmd = CommandBuilder::new("tmux");
    cmd.args(["attach-session", "-t", &tmux_name]);
    cmd.env("TERM", "xterm-256color");
    // Allow nesting: unset TMUX so the attach works when aoe serve runs inside tmux
    cmd.env_remove("TMUX");

    let mut child = match pair.slave.spawn_command(cmd) {
        Ok(child) => child,
        Err(e) => {
            error!(
                target: "terminal.ws",
                client = %client_id,
                tmux = %tmux_name,
                "spawn tmux attach-session failed: {}", e
            );
            close_early(
                &mut socket,
                CLOSE_CODE_INTERNAL_ERROR,
                "attach_spawn_failed",
            )
            .await;
            return;
        }
    };
    debug!(
        target: "terminal.ws",
        client = %client_id,
        tmux = %tmux_name,
        "tmux attach-session spawned"
    );

    // We're done with the slave side
    drop(pair.slave);

    let master = pair.master;

    // Get reader and writer from the PTY master. On failure we must kill
    // and reap the child we just spawned, otherwise the tmux attach
    // process and its slave PTY descriptor leak until exit.
    let mut reader = match master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            error!(
                target: "terminal.ws",
                client = %client_id,
                tmux = %tmux_name,
                "clone PTY reader failed, killing tmux child: {}", e
            );
            let _ = child.kill();
            let _ = child.wait();
            close_early(&mut socket, CLOSE_CODE_INTERNAL_ERROR, "pty_reader_failed").await;
            return;
        }
    };

    let writer = match master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            error!(
                target: "terminal.ws",
                client = %client_id,
                tmux = %tmux_name,
                "take PTY writer failed, killing tmux child: {}", e
            );
            let _ = child.kill();
            let _ = child.wait();
            close_early(&mut socket, CLOSE_CODE_INTERNAL_ERROR, "pty_writer_failed").await;
            return;
        }
    };

    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Share the writer behind a mutex for the input task
    let writer = Arc::new(std::sync::Mutex::new(writer));
    // Share master for resize operations
    let master = Arc::new(std::sync::Mutex::new(master));

    // Use tokio channels to bridge sync PTY I/O with async WebSocket
    let (output_tx, mut output_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    // Control channel: server -> client JSON messages (primary status, etc.)
    let (ctrl_tx, mut ctrl_rx) = tokio::sync::mpsc::channel::<String>(8);

    // Task 1: PTY stdout -> channel (blocking read in dedicated thread)
    let reader_client = client_id.clone();
    let reader_tmux = tmux_name.clone();
    tokio::task::spawn_blocking(move || {
        let mut buf = [0u8; 4096];
        let mut total_bytes: u64 = 0;
        let exit_reason: &'static str;
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    exit_reason = "pty_eof";
                    break;
                }
                Ok(n) => {
                    total_bytes += n as u64;
                    if trace_bytes {
                        trace!(
                            target: "terminal.ws.bytes",
                            client = %reader_client,
                            tmux = %reader_tmux,
                            dir = "pty->ws",
                            bytes = n,
                            "pty read"
                        );
                    }
                    if output_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        exit_reason = "ws_closed";
                        break; // receiver dropped (WebSocket closed)
                    }
                }
                Err(e) => {
                    warn!(
                        target: "terminal.ws",
                        client = %reader_client,
                        tmux = %reader_tmux,
                        "pty read error after {} bytes: {}", total_bytes, e
                    );
                    exit_reason = "pty_read_error";
                    break;
                }
            }
        }
        debug!(
            target: "terminal.ws",
            client = %reader_client,
            tmux = %reader_tmux,
            reason = exit_reason,
            bytes = total_bytes,
            "pty reader task exiting"
        );
    });

    // Task 2: PTY output + control messages -> WebSocket sender
    let send_client = client_id.clone();
    let send_tmux = tmux_name.clone();
    let send_shutdown = shutdown.clone();
    let send_handle = tokio::spawn(async move {
        let mut ping_interval = tokio::time::interval(PING_INTERVAL);
        ping_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // Tokio fires the first interval tick immediately; consume it so
        // we don't ping at connection time (the browser hasn't even
        // attached its onmessage handler yet on first connect).
        ping_interval.tick().await;
        let exit_reason: &'static str;
        loop {
            tokio::select! {
                _ = send_shutdown.cancelled() => {
                    debug!(
                        target: "terminal.ws",
                        client = %send_client,
                        tmux = %send_tmux,
                        "shutdown signaled, send task exiting"
                    );
                    exit_reason = "shutdown";
                    break;
                }
                data = output_rx.recv() => {
                    match data {
                        Some(data) => {
                            let n = data.len();
                            if ws_sender.send(Message::Binary(data.into())).await.is_err() {
                                warn!(
                                    target: "terminal.ws",
                                    client = %send_client,
                                    tmux = %send_tmux,
                                    "ws send (binary) failed, peer gone"
                                );
                                exit_reason = "ws_send_error_binary";
                                break;
                            }
                            if trace_bytes {
                                trace!(
                                    target: "terminal.ws.bytes",
                                    client = %send_client,
                                    tmux = %send_tmux,
                                    dir = "ws->client",
                                    bytes = n,
                                    "ws send binary"
                                );
                            }
                        }
                        None => {
                            debug!(
                                target: "terminal.ws",
                                client = %send_client,
                                tmux = %send_tmux,
                                "pty output channel closed (reader task exited)"
                            );
                            exit_reason = "pty_output_channel_closed";
                            break;
                        }
                    }
                }
                msg = ctrl_rx.recv() => {
                    match msg {
                        Some(text) => {
                            if ws_sender.send(Message::Text(text.into())).await.is_err() {
                                warn!(
                                    target: "terminal.ws",
                                    client = %send_client,
                                    tmux = %send_tmux,
                                    "ws send (control text) failed, peer gone"
                                );
                                exit_reason = "ws_send_error_ctrl";
                                break;
                            }
                        }
                        None => {
                            debug!(
                                target: "terminal.ws",
                                client = %send_client,
                                tmux = %send_tmux,
                                "control channel closed (recv task dropped sender)"
                            );
                            exit_reason = "ctrl_channel_closed";
                            break;
                        }
                    }
                }
                _ = ping_interval.tick() => {
                    // Empty payload: we only care that the client echoes a
                    // Pong, not what's in it. axum's WebSocket forwards
                    // client Pongs to the recv loop, which resets its
                    // idle timer.
                    if ws_sender.send(Message::Ping(Vec::new().into())).await.is_err() {
                        warn!(
                            target: "terminal.ws",
                            client = %send_client,
                            tmux = %send_tmux,
                            "ws send Ping failed, peer gone"
                        );
                        exit_reason = "ws_ping_error";
                        break;
                    }
                    trace!(
                        target: "terminal.ws",
                        client = %send_client,
                        tmux = %send_tmux,
                        "sent keepalive Ping"
                    );
                }
            }
        }
        debug!(
            target: "terminal.ws",
            client = %send_client,
            tmux = %send_tmux,
            reason = exit_reason,
            "send task exiting, sending Close frame"
        );
        // Tell the browser to stop retrying when the PTY relay died
        // (tmux attach failed, pane was killed, etc.). The auto-respawn
        // at WS upgrade catches the common cases; an exit here that
        // close on the heels of the upgrade is almost always a
        // permanently broken pane that retrying won't fix. See #1107.
        let close_frame = match exit_reason {
            "pty_output_channel_closed" => Some(CloseFrame {
                code: CLOSE_CODE_PTY_DEAD,
                reason: "pty_dead".into(),
            }),
            "shutdown" => Some(CloseFrame {
                code: CLOSE_CODE_GOING_AWAY,
                reason: "server shutdown".into(),
            }),
            _ => None,
        };
        let _ = ws_sender.send(Message::Close(close_frame)).await;
    });

    // Task 3: WebSocket receiver -> PTY stdin (and resize)
    let writer_for_input = writer.clone();
    let master_for_resize = master.clone();
    let primaries_for_recv = primaries.clone();
    let client_id_for_recv = client_id.clone();
    let tmux_name_for_recv = tmux_name.clone();
    let ctrl_tx_for_recv = ctrl_tx.clone();
    let pause_counts_for_recv = pause_counts.clone();
    // Track whether THIS ws currently contributes to the pause refcount,
    // shared between the receive loop and the post-loop cleanup.
    let this_ws_paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let this_ws_paused_for_recv = this_ws_paused.clone();

    let recv_client = client_id.clone();
    let recv_tmux = tmux_name.clone();
    let recv_shutdown = shutdown.clone();
    let recv_handle = tokio::spawn(async move {
        // Track the last resize the browser requested so we can apply it
        // when this client becomes primary.
        let mut pending_size: Option<(u16, u16)> = None;
        let exit_reason: &'static str;

        loop {
            // Wrap each receive in IDLE_TIMEOUT so a silent socket
            // (mobile backgrounded, WiFi vanished without TCP RST) is
            // detected and the cleanup path below can kill the tmux
            // child + free its PTY pair. Server-originated Pings in the
            // send task elicit Pongs that arrive here and reset the
            // timer, so live-but-idle sessions stay open indefinitely.
            //
            // The outer `select!` also races against the daemon's
            // shutdown token so a SIGINT/SIGTERM tears this task down
            // promptly instead of waiting for the next client message
            // or the idle timeout. See #1198.
            let timeout_result = tokio::select! {
                _ = recv_shutdown.cancelled() => {
                    debug!(
                        target: "terminal.ws",
                        client = %recv_client,
                        tmux = %recv_tmux,
                        "shutdown signaled, recv task exiting"
                    );
                    exit_reason = "shutdown";
                    break;
                }
                next = tokio::time::timeout(IDLE_TIMEOUT, ws_receiver.next()) => next,
            };
            let msg = match timeout_result {
                Err(_) => {
                    warn!(
                        target: "terminal.ws",
                        client = %recv_client,
                        tmux = %recv_tmux,
                        idle_timeout_secs = IDLE_TIMEOUT.as_secs(),
                        "ws idle reaper fired (no client traffic, including Pongs)"
                    );
                    exit_reason = "idle_reaper";
                    break;
                }
                Ok(None) => {
                    debug!(
                        target: "terminal.ws",
                        client = %recv_client,
                        tmux = %recv_tmux,
                        "ws stream returned None (peer closed cleanly)"
                    );
                    exit_reason = "ws_stream_end";
                    break;
                }
                Ok(Some(Err(e))) => {
                    warn!(
                        target: "terminal.ws",
                        client = %recv_client,
                        tmux = %recv_tmux,
                        "ws recv error: {}", e
                    );
                    exit_reason = "ws_recv_error";
                    break;
                }
                Ok(Some(Ok(msg))) => msg,
            };
            match msg {
                Message::Binary(data) => {
                    // Raw bytes from xterm.js -> PTY stdin (blocked in read-only mode)
                    if read_only {
                        if trace_bytes {
                            trace!(
                                target: "terminal.ws.bytes",
                                client = %recv_client,
                                tmux = %recv_tmux,
                                bytes = data.len(),
                                "dropped client binary (read_only)"
                            );
                        }
                        continue;
                    }

                    if trace_bytes {
                        trace!(
                            target: "terminal.ws.bytes",
                            client = %recv_client,
                            tmux = %recv_tmux,
                            dir = "client->pty",
                            bytes = data.len(),
                            "client binary"
                        );
                    }

                    // Claim primary on input. If we just became primary,
                    // apply our pending resize so the PTY matches our viewport.
                    let became_primary = claim_primary(
                        &primaries_for_recv,
                        &tmux_name_for_recv,
                        &client_id_for_recv,
                    )
                    .await;
                    if became_primary {
                        debug!(
                            target: "terminal.ws",
                            client = %recv_client,
                            tmux = %recv_tmux,
                            "claimed primary on binary input"
                        );
                        if let Some((cols, rows)) = pending_size {
                            resize_pty(&master_for_resize, cols, rows).await;
                        }
                        let _ = ctrl_tx_for_recv
                            .send(r#"{"type":"primary_status","is_primary":true}"#.into())
                            .await;
                    }

                    let writer = writer_for_input.clone();
                    let _ = tokio::task::spawn_blocking(move || {
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_all(&data);
                            let _ = w.flush();
                        }
                    })
                    .await;
                }
                Message::Text(text) => {
                    // Stamp arrival before parse so a timing_ping can report
                    // the server's own parse-to-enqueue cost. Cheap, and text
                    // control frames are rare (not per-keystroke).
                    let text_recv = Instant::now();
                    // JSON control messages (resize, activate) are always allowed
                    if let Ok(control) = serde_json::from_str::<ControlMessage>(&text) {
                        match control {
                            ControlMessage::Resize { cols, rows } if cols > 0 && rows > 0 => {
                                debug!(
                                    target: "terminal.ws",
                                    client = %recv_client,
                                    tmux = %recv_tmux,
                                    cols,
                                    rows,
                                    "control: resize"
                                );
                                pending_size = Some((cols, rows));

                                let dominated = is_primary_or_vacant(
                                    &primaries_for_recv,
                                    &tmux_name_for_recv,
                                    &client_id_for_recv,
                                )
                                .await;
                                if dominated {
                                    resize_pty(&master_for_resize, cols, rows).await;
                                } else {
                                    let _ = ctrl_tx_for_recv
                                        .send(
                                            r#"{"type":"primary_status","is_primary":false}"#
                                                .into(),
                                        )
                                        .await;
                                }
                            }
                            // Ignore zero-dimension resize (buggy client)
                            ControlMessage::Resize { cols, rows } => {
                                warn!(
                                    target: "terminal.ws",
                                    client = %recv_client,
                                    tmux = %recv_tmux,
                                    cols,
                                    rows,
                                    "control: ignoring zero-dimension resize"
                                );
                            }
                            ControlMessage::Activate => {
                                debug!(
                                    target: "terminal.ws",
                                    client = %recv_client,
                                    tmux = %recv_tmux,
                                    "control: activate"
                                );
                                // In read-only mode, don't claim primary. All
                                // viewers get independent resize (vacant state).
                                if read_only {
                                    continue;
                                }
                                let became_primary = claim_primary(
                                    &primaries_for_recv,
                                    &tmux_name_for_recv,
                                    &client_id_for_recv,
                                )
                                .await;
                                if became_primary {
                                    if let Some((cols, rows)) = pending_size {
                                        resize_pty(&master_for_resize, cols, rows).await;
                                    }
                                    let _ = ctrl_tx_for_recv
                                        .send(
                                            r#"{"type":"primary_status","is_primary":true}"#.into(),
                                        )
                                        .await;
                                }
                            }
                            ControlMessage::PauseOutput => {
                                // Client entered tmux scrollback. SIGSTOP the
                                // pane's process tree so it stops emitting
                                // bytes that would shift scrollback under the
                                // reader. Skipped in read-only mode.
                                //
                                // Refcounted across clients: only the first
                                // pause actually SIGSTOPs; duplicate pauses
                                // from the same ws are idempotent so a client
                                // that sends two pause_outputs doesn't
                                // over-contribute to the count.
                                debug!(
                                    target: "terminal.ws",
                                    client = %recv_client,
                                    tmux = %recv_tmux,
                                    "control: pause_output"
                                );
                                if read_only {
                                    continue;
                                }
                                pause_enter(
                                    &pause_counts_for_recv,
                                    &tmux_name_for_recv,
                                    &this_ws_paused_for_recv,
                                )
                                .await;
                            }
                            ControlMessage::ResumeOutput => {
                                // Decrement this ws's contribution; only
                                // SIGCONT when the refcount reaches 0. Safe
                                // to call even if this ws wasn't paused.
                                debug!(
                                    target: "terminal.ws",
                                    client = %recv_client,
                                    tmux = %recv_tmux,
                                    "control: resume_output"
                                );
                                pause_exit(
                                    &pause_counts_for_recv,
                                    &tmux_name_for_recv,
                                    &this_ws_paused_for_recv,
                                )
                                .await;
                            }
                            ControlMessage::TimingPing { seq, client_t } => {
                                // Bounce straight back over the control
                                // channel. Never touches the PTY, never
                                // logs (a sub-second cadence would flood the
                                // log). server_busy_us captures parse +
                                // dispatch + serialize on this task.
                                let pong = TimingPong {
                                    kind: "timing_pong",
                                    seq,
                                    client_t,
                                    server_busy_us: text_recv.elapsed().as_micros() as u64,
                                };
                                if let Ok(text) = serde_json::to_string(&pong) {
                                    let _ = ctrl_tx_for_recv.send(text).await;
                                }
                            }
                        }
                    } else if !read_only {
                        if trace_bytes {
                            trace!(
                                target: "terminal.ws.bytes",
                                client = %recv_client,
                                tmux = %recv_tmux,
                                dir = "client->pty",
                                bytes = text.len(),
                                "client text (non-control)"
                            );
                        }
                        // Plain text input -> PTY stdin (blocked in read-only mode).
                        // Also claims primary, same as binary input.
                        let became_primary = claim_primary(
                            &primaries_for_recv,
                            &tmux_name_for_recv,
                            &client_id_for_recv,
                        )
                        .await;
                        if became_primary {
                            if let Some((cols, rows)) = pending_size {
                                resize_pty(&master_for_resize, cols, rows).await;
                            }
                            let _ = ctrl_tx_for_recv
                                .send(r#"{"type":"primary_status","is_primary":true}"#.into())
                                .await;
                        }

                        let writer = writer_for_input.clone();
                        let bytes: Vec<u8> = text.as_bytes().to_vec();
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Ok(mut w) = writer.lock() {
                                let _ = w.write_all(&bytes);
                                let _ = w.flush();
                            }
                        })
                        .await;
                    }
                }
                Message::Close(frame) => {
                    let (code, reason) = match frame {
                        Some(cf) => (Some(cf.code), cf.reason.to_string()),
                        None => (None, String::new()),
                    };
                    debug!(
                        target: "terminal.ws",
                        client = %recv_client,
                        tmux = %recv_tmux,
                        code = ?code,
                        reason = %reason,
                        "client sent Close frame"
                    );
                    exit_reason = "client_close";
                    break;
                }
                Message::Ping(_) => {
                    trace!(
                        target: "terminal.ws",
                        client = %recv_client,
                        tmux = %recv_tmux,
                        "received Ping (axum auto-replies with Pong)"
                    );
                }
                Message::Pong(_) => {
                    trace!(
                        target: "terminal.ws",
                        client = %recv_client,
                        tmux = %recv_tmux,
                        "received Pong (resets idle timer)"
                    );
                }
            }
        }
        debug!(
            target: "terminal.ws",
            client = %recv_client,
            tmux = %recv_tmux,
            reason = exit_reason,
            "recv task exiting"
        );
    });

    // Wait for either direction to finish
    let exit_side = tokio::select! {
        _ = send_handle => "send",
        _ = recv_handle => "recv",
    };
    let elapsed = started_at.elapsed();
    info!(
        target: "terminal.ws",
        client = %client_id,
        tmux = %tmux_name,
        exit_side,
        elapsed_secs = elapsed.as_secs(),
        elapsed_ms = elapsed.as_millis() as u64,
        "ws session ended, cleaning up"
    );

    // Release primary if this client held it, so the next client can take over.
    release_primary(&primaries, &tmux_name, &client_id).await;

    // Safety net: if this client paused the pane but the WebSocket
    // disconnected before it sent ResumeOutput (WiFi blip, app close,
    // etc.), decrement our contribution to the refcount. If this was
    // the last pauser on the session, SIGCONT the pane's process tree.
    pause_exit(&pause_counts, &tmux_name, &this_ws_paused).await;

    // Clean up: kill the tmux attach process
    let _ = child.kill();
    let _ = child.wait();
    debug!(
        target: "terminal.ws",
        client = %client_id,
        tmux = %tmux_name,
        "tmux attach child reaped, ws handler done"
    );
}

/// Claim primary for this client. Returns `true` if the client was NOT
/// already primary (i.e. it just became primary and should apply its
/// pending resize).
async fn claim_primary(primaries: &SessionPrimaries, session: &str, client_id: &str) -> bool {
    let mut map = primaries.write().await;
    let current = map.get(session);
    if current.is_some_and(|id| id == client_id) {
        return false; // already primary
    }
    map.insert(session.to_string(), client_id.to_string());
    true
}

/// Check whether this client is primary, or no client is primary for
/// this session (vacant). In either case the caller is allowed to resize.
async fn is_primary_or_vacant(
    primaries: &SessionPrimaries,
    session: &str,
    client_id: &str,
) -> bool {
    let map = primaries.read().await;
    match map.get(session) {
        None => true,
        Some(id) => id == client_id,
    }
}

/// Register that THIS WebSocket is asking the pane to be paused.
/// Idempotent per-ws (a duplicate pause_output from the same client
/// does not double-count). SIGSTOPs the pane's process tree on the
/// refcount 0→1 transition. Fire-and-forget: does not await the
/// spawn_blocking so the ws receive loop is not delayed by a slow
/// `tmux display-message`.
async fn pause_enter(
    pause_counts: &SessionPauseCounts,
    session: &str,
    this_ws_paused: &std::sync::atomic::AtomicBool,
) {
    if this_ws_paused.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return; // already contributing; idempotent
    }
    let should_stop = {
        let mut map = pause_counts.lock().await;
        let count = map.entry(session.to_string()).or_insert(0);
        *count += 1;
        *count == 1
    };
    if should_stop {
        let session = session.to_string();
        tokio::task::spawn_blocking(move || {
            if let Some(pid) = crate::process::get_pane_pid(&session) {
                crate::process::stop_process_tree(pid);
            }
        });
    }
}

/// Decrement THIS WebSocket's contribution to the pause refcount.
/// SIGCONTs the pane's process tree on the N→0 transition. Safe to
/// call even if this ws wasn't paused (no-op).
async fn pause_exit(
    pause_counts: &SessionPauseCounts,
    session: &str,
    this_ws_paused: &std::sync::atomic::AtomicBool,
) {
    if !this_ws_paused.swap(false, std::sync::atomic::Ordering::Relaxed) {
        return; // not paused by us
    }
    let should_cont = {
        let mut map = pause_counts.lock().await;
        if let Some(count) = map.get_mut(session) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                map.remove(session);
                true
            } else {
                false
            }
        } else {
            // Refcount underflow shouldn't happen; SIGCONT as safety.
            true
        }
    };
    if should_cont {
        let session = session.to_string();
        tokio::task::spawn_blocking(move || {
            if let Some(pid) = crate::process::get_pane_pid(&session) {
                crate::process::continue_process_tree(pid);
            }
        });
    }
}

/// Release primary if this client currently holds it.
async fn release_primary(primaries: &SessionPrimaries, session: &str, client_id: &str) {
    let mut map = primaries.write().await;
    if map.get(session).is_some_and(|id| id == client_id) {
        map.remove(session);
    }
}

/// Resize the PTY master to the given dimensions.
async fn resize_pty(
    master: &Arc<std::sync::Mutex<Box<dyn portable_pty::MasterPty + Send>>>,
    cols: u16,
    rows: u16,
) {
    let master = master.clone();
    let _ = tokio::task::spawn_blocking(move || {
        if let Ok(m) = master.lock() {
            let _ = m.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
    })
    .await;
}

/// Send a Close frame on an early-return path that hasn't split the
/// socket yet. The `let _ = ` discards send errors: if the peer is
/// already gone the close frame is moot, and we're returning anyway.
async fn close_early(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = socket
        .send(Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })))
        .await;
}

/// Outcome of one tmux-readiness probe. `Ready` lets the caller proceed
/// to `tmux attach-session`; `NotReady` means try again after the poll
/// interval; `Dead` short-circuits the wait when every pane is reported
/// dead (no point in polling further).
#[derive(Debug, PartialEq, Eq)]
enum PaneReadiness {
    Ready,
    NotReady,
    Dead,
}

/// Parse `tmux list-panes -F "#{pane_dead}"` output: one line per pane,
/// each line `0` (alive) or `1` (dead). Empty output means the session
/// exists but has no panes yet (not ready). All-dead means the pane has
/// permanently exited.
fn parse_pane_dead_output(output: &str) -> PaneReadiness {
    let lines: Vec<&str> = output
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if lines.is_empty() {
        return PaneReadiness::NotReady;
    }
    if lines.contains(&"0") {
        PaneReadiness::Ready
    } else {
        PaneReadiness::Dead
    }
}

/// Poll `tmux has-session` + `tmux list-panes` at TMUX_READY_POLL until
/// the session has at least one alive pane, or until TMUX_READY_TIMEOUT
/// expires. Returns the final outcome so the caller can distinguish a
/// transient warm-up (`NotReady` -> retryable 1013) from a permanently
/// dead pane (`Dead` -> 4001 short-circuit). Bails out early on `Dead`
/// rather than polling further because no amount of waiting will make
/// an exited pane reattachable.
async fn wait_for_tmux_ready(tmux_name: &str) -> PaneReadiness {
    let deadline = Instant::now() + TMUX_READY_TIMEOUT;
    loop {
        match probe_tmux_readiness(tmux_name).await {
            PaneReadiness::Ready => return PaneReadiness::Ready,
            PaneReadiness::Dead => return PaneReadiness::Dead,
            PaneReadiness::NotReady => {
                if Instant::now() >= deadline {
                    return PaneReadiness::NotReady;
                }
                tokio::time::sleep(TMUX_READY_POLL).await;
            }
        }
    }
}

/// One probe iteration: `tmux has-session` then (on success) `tmux
/// list-panes -F "#{pane_dead}"`. Both shell out to the tmux binary;
/// they're cheap (microseconds in the happy path) so the 50ms poll
/// floor dominates wall time, not subprocess overhead.
async fn probe_tmux_readiness(tmux_name: &str) -> PaneReadiness {
    let name = tmux_name.to_string();
    tokio::task::spawn_blocking(move || {
        let has_session = std::process::Command::new("tmux")
            .args(["has-session", "-t", &name])
            .output();
        let has_session_ok = match has_session {
            Ok(o) => o.status.success(),
            Err(_) => false,
        };
        if !has_session_ok {
            return PaneReadiness::NotReady;
        }
        let panes = std::process::Command::new("tmux")
            .args(["list-panes", "-t", &name, "-F", "#{pane_dead}"])
            .output();
        match panes {
            Ok(o) if o.status.success() => {
                parse_pane_dead_output(&String::from_utf8_lossy(&o.stdout))
            }
            _ => PaneReadiness::NotReady,
        }
    })
    .await
    .unwrap_or(PaneReadiness::NotReady)
}

#[derive(serde::Deserialize)]
#[serde(tag = "type")]
enum ControlMessage {
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
    /// Sent by the browser when the terminal tab/window gains focus.
    /// Claims primary and applies the pending resize so the pane
    /// snaps to this client's viewport without requiring a keystroke.
    #[serde(rename = "activate")]
    Activate,
    /// Pause the pane's foreground process (SIGSTOP). Sent by mobile
    /// web clients when the user enters tmux scrollback — without
    /// pausing, claude's continued output keeps shifting scrollback
    /// under the reader. Paired with `resume_output` on exit. The
    /// server also auto-resumes on WebSocket close to prevent a
    /// dropped connection from leaving the process permanently
    /// suspended.
    #[serde(rename = "pause_output")]
    PauseOutput,
    /// Resume the pane's foreground process (SIGCONT). Inverse of
    /// `pause_output`. SIGCONT to a non-stopped process is a no-op.
    #[serde(rename = "resume_output")]
    ResumeOutput,
    /// Latency probe from the web terminal under `?debug=terminal-timing`.
    /// Bounced straight back as a `timing_pong` over the control channel,
    /// never reaching the PTY. `client_t` is an opaque client timestamp
    /// echoed unchanged so the browser can compute the round trip. The
    /// variant is always parseable but only exercised by debug clients, so
    /// normal sessions pay nothing beyond one extra serde tag. See #1453.
    #[serde(rename = "timing_ping")]
    TimingPing { seq: u64, client_t: f64 },
}

/// Server reply to a [`ControlMessage::TimingPing`]. `server_busy_us` is
/// this handler's own parse-to-enqueue duration, so the client can
/// subtract it from the observed round trip to isolate network plus
/// WebSocket transit without any client/server clock sync. See #1453.
#[derive(serde::Serialize)]
struct TimingPong {
    #[serde(rename = "type")]
    kind: &'static str,
    seq: u64,
    client_t: f64,
    server_busy_us: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_primaries() -> SessionPrimaries {
        Arc::new(RwLock::new(std::collections::HashMap::new()))
    }

    #[tokio::test]
    async fn claim_primary_vacant_returns_true() {
        let p = make_primaries();
        assert!(claim_primary(&p, "session-1", "ws-0").await);
        assert_eq!(p.read().await.get("session-1").unwrap(), "ws-0");
    }

    #[tokio::test]
    async fn claim_primary_already_primary_returns_false() {
        let p = make_primaries();
        assert!(claim_primary(&p, "session-1", "ws-0").await);
        assert!(!claim_primary(&p, "session-1", "ws-0").await);
    }

    #[tokio::test]
    async fn claim_primary_steals_from_other_client() {
        let p = make_primaries();
        assert!(claim_primary(&p, "session-1", "ws-0").await);
        assert!(claim_primary(&p, "session-1", "ws-1").await);
        assert_eq!(p.read().await.get("session-1").unwrap(), "ws-1");
    }

    #[tokio::test]
    async fn is_primary_or_vacant_when_vacant() {
        let p = make_primaries();
        assert!(is_primary_or_vacant(&p, "session-1", "ws-0").await);
    }

    #[tokio::test]
    async fn is_primary_or_vacant_when_primary() {
        let p = make_primaries();
        claim_primary(&p, "session-1", "ws-0").await;
        assert!(is_primary_or_vacant(&p, "session-1", "ws-0").await);
    }

    #[tokio::test]
    async fn is_primary_or_vacant_when_not_primary() {
        let p = make_primaries();
        claim_primary(&p, "session-1", "ws-0").await;
        assert!(!is_primary_or_vacant(&p, "session-1", "ws-1").await);
    }

    #[tokio::test]
    async fn release_primary_clears_entry() {
        let p = make_primaries();
        claim_primary(&p, "session-1", "ws-0").await;
        release_primary(&p, "session-1", "ws-0").await;
        assert!(p.read().await.get("session-1").is_none());
    }

    #[tokio::test]
    async fn release_primary_noop_for_different_client() {
        let p = make_primaries();
        claim_primary(&p, "session-1", "ws-0").await;
        release_primary(&p, "session-1", "ws-1").await;
        assert_eq!(p.read().await.get("session-1").unwrap(), "ws-0");
    }

    #[tokio::test]
    async fn release_primary_noop_on_empty_map() {
        let p = make_primaries();
        release_primary(&p, "session-1", "ws-0").await;
        assert!(p.read().await.is_empty());
    }

    #[test]
    fn parse_pane_dead_empty_is_not_ready() {
        // `list-panes` succeeded but the session has no panes yet
        // (transient state during tmux session creation).
        assert_eq!(parse_pane_dead_output(""), PaneReadiness::NotReady);
        assert_eq!(parse_pane_dead_output("   \n\n  "), PaneReadiness::NotReady);
    }

    #[test]
    fn parse_pane_dead_single_alive_is_ready() {
        assert_eq!(parse_pane_dead_output("0\n"), PaneReadiness::Ready);
        assert_eq!(parse_pane_dead_output("0"), PaneReadiness::Ready);
    }

    #[test]
    fn parse_pane_dead_single_dead_is_dead() {
        assert_eq!(parse_pane_dead_output("1\n"), PaneReadiness::Dead);
    }

    #[test]
    fn parse_pane_dead_mixed_is_ready() {
        // One alive pane is enough; tmux attach finds the alive one.
        assert_eq!(parse_pane_dead_output("1\n0\n"), PaneReadiness::Ready);
        assert_eq!(parse_pane_dead_output("0\n1\n1\n"), PaneReadiness::Ready);
    }

    #[test]
    fn parse_pane_dead_all_dead_is_dead() {
        assert_eq!(parse_pane_dead_output("1\n1\n1\n"), PaneReadiness::Dead);
    }

    #[test]
    fn timing_ping_deserializes() {
        let msg: ControlMessage =
            serde_json::from_str(r#"{"type":"timing_ping","seq":7,"client_t":1234.5}"#).unwrap();
        match msg {
            ControlMessage::TimingPing { seq, client_t } => {
                assert_eq!(seq, 7);
                assert_eq!(client_t, 1234.5);
            }
            _ => panic!("expected TimingPing"),
        }
    }

    #[test]
    fn timing_pong_serializes_with_type_tag() {
        let pong = TimingPong {
            kind: "timing_pong",
            seq: 7,
            client_t: 1234.5,
            server_busy_us: 42,
        };
        let json = serde_json::to_string(&pong).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["type"], "timing_pong");
        assert_eq!(parsed["seq"], 7);
        assert_eq!(parsed["client_t"], 1234.5);
        assert_eq!(parsed["server_busy_us"], 42);
    }

    #[tokio::test]
    async fn independent_sessions_dont_interfere() {
        let p = make_primaries();
        claim_primary(&p, "session-1", "ws-0").await;
        claim_primary(&p, "session-2", "ws-1").await;
        assert_eq!(p.read().await.get("session-1").unwrap(), "ws-0");
        assert_eq!(p.read().await.get("session-2").unwrap(), "ws-1");
    }
}
