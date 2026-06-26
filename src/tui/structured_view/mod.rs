//! Native ratatui rendering of a structured view session.
//!
//! Consumes the same daemon HTTP / WebSocket surface that the web
//! frontend uses; the per-frame reducer mirrors the activity semantics
//! of `web/src/hooks/useAcp.ts` without the React-specific shapes.
//!
//! Directory name is `structured_view` (not `structured view`) to avoid colliding
//! with `src/acp/` per the recipe in
//! https://github.com/agent-of-empires/agent-of-empires/issues/1018#issuecomment-4444040929.

pub mod input;
pub mod mention;
pub mod queue;
pub mod reducer;
pub mod render;
pub mod slash;
pub mod state;

use std::io::Stdout;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyEventKind};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::time::Instant;

use self::input::{Focus, InputContext, Intent};
use self::state::{FileIndex, MentionSession, StructuredViewState, ToastBanner, ToastKind};
use crate::acp::approvals::ApprovalDecision;
use crate::acp::client::{
    require_daemon, ws_connect, DaemonEndpoint, HttpClient, HttpError, ManagerError, WsError,
    WsMessage, REPLAY_PAGE_SIZE,
};
use crate::acp::elicitations::ElicitationResolution;
use crate::acp::protocol::ApprovalDecisionWire;
use crate::plugin::ui_state::{Tone, UiSnapshot};
use crate::session::config::{resolve_theme_name, resolve_theme_palette_mode};
use crate::tui::styles::Theme;

/// Per-keystroke redraw interval. The animations are minimal (just the
/// blinking caret in the composer); 120ms keeps it from looking laggy
/// without burning CPU.
const REDRAW_INTERVAL: Duration = Duration::from_millis(120);
/// Toasts auto-clear after this long.
const TOAST_TTL: Duration = Duration::from_secs(4);
/// How often to poll the daemon's plugin UI-state snapshot (#2402). Matches
/// the web dashboard's cadence. The fetch runs on its own task so a slow or
/// unreachable daemon never blocks the event loop on the HTTP client's
/// 15-second timeout.
const PLUGIN_UI_POLL_INTERVAL: Duration = Duration::from_secs(3);

/// Set up an alternate-screen terminal, run the structured view against
/// the given session, and tear it back down on exit. Used by the
/// `aoe acp attach <id>` CLI verb to jump straight into the
/// structured view without going through the home screen. Pair with
/// `AOE_DAEMON_URL` for remote-attach against another machine's
/// structured view daemon.
pub async fn run_standalone(session_id: &str) -> anyhow::Result<()> {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        EventStream,
    };
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use std::io;
    use std::io::IsTerminal;

    if !io::stdin().is_terminal() {
        anyhow::bail!("stdin is not a terminal; `aoe acp attach` requires an interactive TTY");
    }

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut event_stream = EventStream::new();
    let theme_name = resolve_theme_name();
    let palette_mode = resolve_theme_palette_mode();
    let theme = crate::tui::styles::load_theme_with_mode(&theme_name, palette_mode);

    let result = run(&mut terminal, &mut event_stream, &theme, session_id).await;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;
    result
}

/// Open the structured view for `session_id` and run its event loop until
/// the user exits with `Esc`, or until the structured view daemon becomes
/// unreachable in a way the view can't recover from.
///
/// Borrows the host terminal + event stream so the parent App can
/// resume rendering when the view returns.
pub async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    event_stream: &mut EventStream,
    theme: &Theme,
    session_id: &str,
) -> Result<()> {
    let endpoint = match require_daemon().await {
        Ok(e) => e,
        Err(ManagerError::EnvOverrideUnreachable) => {
            render_error_screen(
                terminal,
                theme,
                "AOE_DAEMON_URL is set but the daemon at that URL is unreachable.\n\nCheck the URL, or unset the env var to use a local daemon.",
            )?;
            wait_for_dismiss(event_stream).await?;
            return Ok(());
        }
        Err(ManagerError::EnvOverrideUnauthorized) => {
            render_error_screen(
                terminal,
                theme,
                "AOE_DAEMON_URL is set but the daemon rejected the bearer token.\n\nCheck AOE_DAEMON_TOKEN.",
            )?;
            wait_for_dismiss(event_stream).await?;
            return Ok(());
        }
        Err(e @ ManagerError::NoDaemonRunning(_)) => {
            // Carries the multi-line "start one with..." hint from the
            // error variant. Render as-is so the user sees the choice
            // between localhost/Tailscale/Cloudflare without having to
            // dig through docs.
            render_error_screen(
                terminal,
                theme,
                &format!("{e}\n\nPress any key to return to the session list."),
            )?;
            wait_for_dismiss(event_stream).await?;
            return Ok(());
        }
    };
    run_for_endpoint(terminal, event_stream, theme, endpoint, session_id).await
}

/// Same as [`run`] but the caller has already located the daemon
/// endpoint (e.g. the remote-home picker that ran a session discovery
/// step against a fixed `AOE_DAEMON_URL`). Skips `require_daemon` so
/// the view doesn't re-run discovery / health-check when the caller
/// has already done it.
pub async fn run_for_endpoint(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    event_stream: &mut EventStream,
    theme: &Theme,
    endpoint: DaemonEndpoint,
    session_id: &str,
) -> Result<()> {
    let http = HttpClient::new(endpoint.clone()).context("build structured view HTTP client")?;

    // Hydrate the transcript via /replay before opening the WebSocket
    // so the user sees the historical conversation immediately instead
    // of a blank pane until live frames start arriving.
    let initial = http.replay_paged(session_id, 0, REPLAY_PAGE_SIZE).await;
    let ws_result = ws_connect(&endpoint, session_id, 0).await;

    let (ws, ws_err) = match ws_result {
        Ok(handle) => (Some(handle), None),
        Err(e) => (None, Some(e)),
    };

    let mut state = StructuredViewState::new(session_id.to_string(), endpoint, http, ws);
    state.focus = Focus::Transcript;

    let mut toast_deadline: Option<Instant> = None;

    // Resolve the queue drain mode from the daemon (not local config:
    // this view can attach to a remote daemon). A failure here is
    // non-fatal; the queue still works, it just uses the default mode.
    match state.http.queue_drain_mode().await {
        Ok(mode) => state.drain_mode = mode,
        Err(e) => {
            tracing::warn!(target: "acp.tui", "queue drain mode fetch failed: {e}");
            set_toast(
                &mut state,
                &mut toast_deadline,
                format!("queue drain mode unknown ({e}); using default"),
                ToastKind::Error,
            );
        }
    }

    // Capture both startup-path errors before showing a toast so we
    // can fold them into a single message when both fail (they
    // usually share a root cause, e.g. 401 from the auth middleware).
    let replay_err = match initial {
        Ok(replay) => {
            if replay.lost {
                state.transcript.set_lagged();
            }
            for frame in &replay.frames {
                state.transcript.apply(frame);
            }
            state.reconcile_selection();
            state.reconcile_slash_selection();
            None
        }
        Err(e) => {
            tracing::warn!(target: "acp.tui", "initial replay failed: {e}");
            Some(e.to_string())
        }
    };

    let ws_err_text = ws_err.map(|e| {
        tracing::warn!(target: "acp.tui.ws", "initial ws connect failed: {e}");
        e.to_string()
    });

    let startup_toast = match (replay_err, ws_err_text) {
        (Some(r), Some(w)) => Some(format!("startup failed: replay={r}; ws={w}")),
        (Some(r), None) => Some(format!("replay failed: {r}")),
        (None, Some(w)) => Some(format!("ws connect failed: {w}")),
        (None, None) => None,
    };

    if let Some(text) = startup_toast {
        set_toast(&mut state, &mut toast_deadline, text, ToastKind::Error);
    }

    redraw(terminal, theme, &state)?;

    let mut redraw_ticker = tokio::time::interval(REDRAW_INTERVAL);
    redraw_ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Poll the daemon's plugin UI-state on its own task and stream snapshots
    // back over a channel, so a slow daemon stalls neither input nor render.
    // The task exits once the view returns and drops the receiver.
    let (plugin_tx, mut plugin_rx) = tokio::sync::mpsc::channel::<UiSnapshot>(8);
    {
        let http = state.http.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(PLUGIN_UI_POLL_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match http.plugin_ui_state().await {
                    Ok(snapshot) => {
                        if plugin_tx.send(snapshot).await.is_err() {
                            break; // view exited; receiver gone.
                        }
                    }
                    // Transient or older-daemon-without-the-endpoint: keep the
                    // last good snapshot and retry on the next tick rather than
                    // toasting repeatedly.
                    Err(e) => {
                        tracing::debug!(target: "acp.tui", "plugin ui-state poll failed: {e}");
                    }
                }
            }
        });
    }

    loop {
        tokio::select! {
            biased;
            evt = event_stream.next() => {
                let Some(evt) = evt else {
                    // EventStream closed; bail out so the parent App
                    // can do its own cleanup.
                    return Ok(());
                };
                let evt = evt.context("read terminal event")?;
                let should_exit = handle_terminal_event(&mut state, evt, &mut toast_deadline).await?;
                if should_exit {
                    return Ok(());
                }
                redraw(terminal, theme, &state)?;
            }
            ws_msg = recv_ws(&mut state) => {
                match ws_msg {
                    Some(Ok(WsMessage::Frame(frame))) => {
                        let was_active = state.transcript.turn_active;
                        state.transcript.apply(&frame);
                        state.reconcile_selection();
                        state.reconcile_slash_selection();
                        let now_active = state.transcript.turn_active;
                        if !was_active && now_active {
                            // Turn started (our own prompt echoed back, or
                            // another client's). The optimistic lock has
                            // served its purpose; release it.
                            state.in_flight = false;
                        } else if was_active && !now_active {
                            // Turn ended: release the lock and drain the
                            // next queued batch, if any.
                            state.in_flight = false;
                            maybe_drain(&mut state, &mut toast_deadline).await;
                        }
                        redraw(terminal, theme, &state)?;
                    }
                    Some(Ok(WsMessage::Lagged)) => {
                        // Daemon evicted events we hadn't seen yet. Drop
                        // local reducer state and rehydrate from /replay.
                        state.transcript.reset();
                        match state
                            .http
                            .replay_paged(&state.session_id, 0, REPLAY_PAGE_SIZE)
                            .await
                        {
                            Ok(replay) => {
                                if replay.lost {
                                    state.transcript.set_lagged();
                                }
                                for frame in &replay.frames {
                                    state.transcript.apply(frame);
                                }
                                state.reconcile_selection();
                                state.reconcile_slash_selection();
                                // Re-derived turn state from the rebuilt
                                // transcript; the lock no longer reflects
                                // anything observable. Drain if idle.
                                state.in_flight = false;
                                maybe_drain(&mut state, &mut toast_deadline).await;
                            }
                            Err(e) => {
                                set_toast(&mut state, &mut toast_deadline, format!("replay failed: {e}"), ToastKind::Error);
                            }
                        }
                        redraw(terminal, theme, &state)?;
                    }
                    Some(Err(e)) => {
                        // WS dropped; show a banner and try to reconnect
                        // from the last seq we processed. Bounded backoff
                        // so a flaky daemon restart (e.g. a 2-second
                        // process bounce) survives without paging the
                        // user, but a permanently-down daemon doesn't
                        // pin a worker tight-looping retries.
                        tracing::warn!(target: "acp.tui.ws", "ws disconnect: {e}");
                        set_toast(&mut state, &mut toast_deadline, format!("ws disconnected: {e}; reconnecting…"), ToastKind::Error);
                        state.ws = None;
                        // Can't observe turn boundaries while the socket
                        // is down; drop the lock so a stuck send doesn't
                        // wedge the composer, and queue any new prompts
                        // (is_busy() is true while ws is None).
                        state.in_flight = false;
                        let since = state.transcript.last_seq;
                        match reconnect_with_backoff(&state.endpoint, &state.session_id, since).await {
                            Ok(handle) => {
                                state.ws = Some(handle);
                                set_toast(&mut state, &mut toast_deadline, "ws reconnected".into(), ToastKind::Info);
                                // Resumed frames will re-derive turn state
                                // and drain on the next edge, but if the
                                // turn already ended before reconnect there
                                // is no edge to wait for: drain now.
                                maybe_drain(&mut state, &mut toast_deadline).await;
                            }
                            Err(e) => {
                                set_toast(&mut state, &mut toast_deadline, format!("ws reconnect failed: {e}"), ToastKind::Error);
                            }
                        }
                        redraw(terminal, theme, &state)?;
                    }
                    None => {
                        // Either no ws handle or the channel closed.
                        // Sleep briefly to avoid spinning the select loop.
                        tokio::time::sleep(Duration::from_millis(200)).await;
                    }
                }
            }
            Some(snapshot) = plugin_rx.recv() => {
                state.ingest_plugin_ui(snapshot);
                drain_plugin_toast(&mut state, &mut toast_deadline);
                redraw(terminal, theme, &state)?;
            }
            _ = redraw_ticker.tick() => {
                let now = Instant::now();
                if let Some(deadline) = toast_deadline {
                    if now >= deadline {
                        state.toast = None;
                        toast_deadline = None;
                    }
                }
                // A freed slot lets the next buffered plugin notification show.
                drain_plugin_toast(&mut state, &mut toast_deadline);
                redraw(terminal, theme, &state)?;
            }
        }
    }
}

/// Show the next buffered plugin notification as a toast, but only when no
/// toast is currently up, so app toasts (errors, send confirmations) are not
/// pre-empted and queued notifications show one at a time.
fn drain_plugin_toast(state: &mut StructuredViewState, toast_deadline: &mut Option<Instant>) {
    if state.toast.is_some() {
        return;
    }
    let Some(n) = state.next_plugin_toast() else {
        return;
    };
    let kind = match n.tone {
        Tone::Warn | Tone::Danger => ToastKind::Error,
        _ => ToastKind::Info,
    };
    let text = match &n.body {
        Some(body) => format!("{}: {body}", n.title),
        None => n.title.clone(),
    };
    set_toast(state, toast_deadline, text, kind);
}

async fn handle_terminal_event(
    state: &mut StructuredViewState,
    evt: CrosstermEvent,
    toast_deadline: &mut Option<Instant>,
) -> Result<bool> {
    let CrosstermEvent::Key(key) = evt else {
        return Ok(false);
    };
    // Skip key-release events on terminals that emit them (Windows
    // crossterm, kitty enhanced protocol). Otherwise every keypress
    // triggers two handle_key calls.
    if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
        return Ok(false);
    }

    let has_pending = !state.transcript.pending_approvals.is_empty();
    let ctx = InputContext {
        has_pending_approval: has_pending,
        has_pending_elicitation: !state.transcript.pending_elicitations.is_empty(),
        slash_picker_open: state.slash_picker_open(),
        mention_picker_open: state.mention.is_some(),
        caret_at_origin: state.caret_at_origin(),
        browsing_queue: state.browsing_queue(),
        queue_len: state.queue.len(),
    };
    let intent = input::dispatch(state.focus, &key, ctx);
    match intent {
        Intent::Ignore => Ok(false),
        Intent::Exit => Ok(true),
        Intent::SetFocus(focus) => {
            // Approval focus only makes sense when there's one to
            // select; otherwise fall through to transcript.
            state.focus = if matches!(focus, Focus::Approval) && !has_pending {
                Focus::Transcript
            } else {
                focus
            };
            // Leaving the composer ends any queue-recall browse; the
            // in-progress text stays put as a draft.
            if state.focus != Focus::Composer {
                state.cancel_recall();
            }
            state.reconcile_selection();
            Ok(false)
        }
        Intent::Compose(k) => {
            // ratatui_textarea consumes raw crossterm KeyEvent through
            // its `Input` conversion. Snapshot the slash query first so
            // we can detect a query-text change (vs. mere cursor motion)
            // and reset the picker highlight only when the text shifts.
            let before = state.slash_query();
            state.composer.input(k);
            if state.slash_query() != before {
                state.slash_selected = 0;
            }
            state.reconcile_slash_selection();
            // The typed text may have opened, narrowed, or closed an
            // `@`-mention; recompute and fetch the file list on first open.
            refresh_mention(state);
            ensure_files_loaded(state, toast_deadline).await;
            Ok(false)
        }
        Intent::SlashMove(delta) => {
            state.move_slash_selection(delta);
            Ok(false)
        }
        Intent::SlashAccept => {
            state.accept_selected_slash();
            Ok(false)
        }
        Intent::SlashDismiss => {
            state.dismiss_slash();
            Ok(false)
        }
        Intent::MentionNavigate(delta) => {
            navigate_mention(state, delta);
            Ok(false)
        }
        Intent::MentionAccept => {
            accept_mention(state);
            Ok(false)
        }
        Intent::MentionClose => {
            // Remember the dismissed anchor so the picker stays shut while
            // the user keeps typing in this same token.
            state.dismissed_mention =
                mention::active_mention(state.composer.lines(), composer_cursor(state))
                    .map(|m| (m.row, m.start_col));
            state.mention = None;
            Ok(false)
        }
        Intent::SubmitPrompt => {
            // Capture the browse target before take_composer_text resets
            // the recall state.
            let recall = state.recall.take();
            let text = state.take_composer_text();
            // Submitting while browsing edits that queued entry in place,
            // preserving its position rather than enqueuing a duplicate.
            // If the entry drained between recall and now, the index is
            // stale; fall through to the normal send / queue path so the
            // edited text is never lost.
            if !text.is_empty() {
                if let Some(r) = recall {
                    if state.queue.replace(r.index, text.clone()) {
                        set_toast(
                            state,
                            toast_deadline,
                            format!("edited queued prompt ({} waiting)", state.queue.len()),
                            ToastKind::Info,
                        );
                        return Ok(false);
                    }
                }
            }
            if text.is_empty() {
                // Empty Enter is a manual flush: if the agent is idle and
                // prompts are stuck in the queue (e.g. a drain POST failed
                // earlier), retry the drain. Otherwise just nudge the user.
                if !state.is_busy() && !state.queue.is_empty() {
                    maybe_drain(state, toast_deadline).await;
                } else {
                    set_toast(
                        state,
                        toast_deadline,
                        "composer is empty".into(),
                        ToastKind::Info,
                    );
                }
                return Ok(false);
            }
            if state.is_busy() {
                // A turn is running (or the socket is down): park the
                // prompt so it drains when the agent next goes idle.
                state.queue.push(text);
                set_toast(
                    state,
                    toast_deadline,
                    format!("queued ({} waiting)", state.queue.len()),
                    ToastKind::Info,
                );
                return Ok(false);
            }
            if send_prompt_now(state, toast_deadline, &text).await {
                set_toast(
                    state,
                    toast_deadline,
                    format!("prompt sent ({} bytes)", text.len()),
                    ToastKind::Info,
                );
            }
            Ok(false)
        }
        Intent::ClearQueue => {
            if state.queue.is_empty() {
                return Ok(false);
            }
            state.queue.clear();
            // The browsed entry no longer exists; end the browse but keep
            // whatever text is in the composer as a draft.
            state.cancel_recall();
            set_toast(
                state,
                toast_deadline,
                "queue cleared".into(),
                ToastKind::Info,
            );
            Ok(false)
        }
        Intent::RecallQueued(delta) => {
            state.recall_step(delta);
            Ok(false)
        }
        Intent::RecallCancel => {
            state.recall_cancel_restore();
            Ok(false)
        }
        Intent::Scroll(delta) => {
            apply_scroll(state, delta);
            Ok(false)
        }
        Intent::ResolveApproval(decision) => {
            let Some(idx) = state.selected_approval else {
                return Ok(false);
            };
            let Some(pending) = state.transcript.pending_approvals.get(idx).cloned() else {
                return Ok(false);
            };
            match state
                .http
                .resolve_approval(&state.session_id, &pending.nonce, decision)
                .await
            {
                Ok(()) => {
                    let label = match decision {
                        ApprovalDecisionWire::Allow => "allowed",
                        ApprovalDecisionWire::AllowAlways => "allow-always",
                        ApprovalDecisionWire::Deny => "denied",
                        ApprovalDecisionWire::Cancelled => "cancelled",
                    };
                    // Clear the card now instead of waiting on the
                    // ApprovalResolved broadcast, which the seq dedupe can
                    // drop and leave the card stuck. See #1821.
                    state
                        .transcript
                        .resolve_approval_locally(&pending.nonce, ApprovalDecision::from(decision));
                    // The selected/last approval may have just disappeared;
                    // re-anchor focus like the replay/live-frame paths do.
                    state.reconcile_selection();
                    set_toast(
                        state,
                        toast_deadline,
                        format!("approval {label}"),
                        ToastKind::Info,
                    );
                }
                // The daemon reports the nonce already gone: the approval
                // resolved server-side (concurrent decision, watchdog
                // cancel, or no matching option). Clear the card without an
                // error toast. See #1821.
                Err(HttpError::ApprovalGone) => {
                    state
                        .transcript
                        .resolve_approval_locally(&pending.nonce, ApprovalDecision::from(decision));
                    state.reconcile_selection();
                    set_toast(
                        state,
                        toast_deadline,
                        "approval already resolved".into(),
                        ToastKind::Info,
                    );
                }
                Err(e) => {
                    set_toast(
                        state,
                        toast_deadline,
                        format!("approval failed: {e}"),
                        ToastKind::Error,
                    );
                }
            }
            Ok(false)
        }
        Intent::SkipElicitation | Intent::CancelElicitation => {
            let Some(pending) = state.transcript.pending_elicitations.first().cloned() else {
                return Ok(false);
            };
            let (resolution, label) = if matches!(intent, Intent::SkipElicitation) {
                (ElicitationResolution::Decline, "question skipped")
            } else {
                (ElicitationResolution::Cancel, "question cancelled")
            };
            match state
                .http
                .resolve_elicitation(&state.session_id, &pending.nonce, &resolution)
                .await
            {
                Ok(()) | Err(HttpError::ApprovalGone) => {
                    // Clear locally now; the ElicitationResolved broadcast
                    // also clears it, but the seq dedupe can swallow that.
                    state.transcript.resolve_elicitation_locally(&pending.nonce);
                    set_toast(state, toast_deadline, label.into(), ToastKind::Info);
                }
                Err(e) => {
                    set_toast(
                        state,
                        toast_deadline,
                        format!("elicitation resolve failed: {e}"),
                        ToastKind::Error,
                    );
                }
            }
            Ok(false)
        }
        Intent::CancelInFlight => {
            match state.http.cancel(&state.session_id).await {
                Ok(()) => set_toast(state, toast_deadline, "cancel sent".into(), ToastKind::Info),
                Err(e) => set_toast(
                    state,
                    toast_deadline,
                    format!("cancel failed: {e}"),
                    ToastKind::Error,
                ),
            }
            Ok(false)
        }
        Intent::OpenInBrowser => {
            let url = format!(
                "{}/sessions/{}/acp",
                state.endpoint.base_url, state.session_id
            );
            if let Err(e) = webbrowser::open(&url) {
                set_toast(
                    state,
                    toast_deadline,
                    format!("open failed: {e}"),
                    ToastKind::Error,
                );
            } else {
                set_toast(
                    state,
                    toast_deadline,
                    "opened in browser".into(),
                    ToastKind::Info,
                );
            }
            Ok(false)
        }
    }
}

/// Async pull from the structured view WebSocket. Returns `None` when no ws
/// handle is currently attached so the select arm degrades to a
/// timed wait instead of busy-looping.
async fn recv_ws(state: &mut StructuredViewState) -> Option<Result<WsMessage, WsError>> {
    let ws = state.ws.as_mut()?;
    ws.recv().await
}

/// Reconnect with three attempts and 250ms / 500ms / 1000ms backoff.
/// Daemon restarts on the same box come back in under a second; a
/// remote daemon failure usually doesn't recover inside our budget,
/// so the user gets a toast and can hit retry themselves.
async fn reconnect_with_backoff(
    endpoint: &DaemonEndpoint,
    session_id: &str,
    since: u64,
) -> Result<crate::acp::client::WsHandle, WsError> {
    const BACKOFFS_MS: &[u64] = &[250, 500, 1000];
    let mut last_err: Option<WsError> = None;
    for (i, &delay) in BACKOFFS_MS.iter().enumerate() {
        if i > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        match ws_connect(endpoint, session_id, since).await {
            Ok(handle) => return Ok(handle),
            Err(e) => {
                tracing::debug!(
                    target: "acp.tui.ws",
                    attempt = i + 1,
                    "ws reconnect attempt failed: {e}"
                );
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("at least one attempt"))
}

/// The composer cursor as a plain `(row, col)` char-index tuple, the
/// shape [`mention::active_mention`] expects.
fn composer_cursor(state: &StructuredViewState) -> (usize, usize) {
    let c = state.composer.cursor();
    (c.0, c.1)
}

/// Recompute the `@`-mention picker from the composer's current text.
/// Opens the picker when the cursor sits in a fresh `@`-token, keeps it
/// open while the token narrows, and closes it when the token goes away
/// or was dismissed with Esc. The query itself is never stored; it is
/// always derived from the textarea so there is one source of truth.
fn refresh_mention(state: &mut StructuredViewState) {
    let active = mention::active_mention(state.composer.lines(), composer_cursor(state));
    match active {
        None => {
            state.mention = None;
            state.dismissed_mention = None;
        }
        Some(m) => {
            let anchor = (m.row, m.start_col);
            if state.dismissed_mention == Some(anchor) {
                // Still inside the token the user dismissed; stay shut.
                state.mention = None;
            } else {
                state.dismissed_mention = None;
                let selected = state.mention.as_ref().map(|s| s.selected).unwrap_or(0);
                state.mention = Some(MentionSession { selected });
            }
        }
    }
}

/// Files currently matching the open mention's query, capped for the
/// picker. Empty when the picker is closed or the index is not loaded.
pub(super) fn filtered_mention_files(state: &StructuredViewState) -> Vec<String> {
    if state.mention.is_none() {
        return Vec::new();
    }
    let FileIndex::Loaded { files, .. } = &state.file_index else {
        return Vec::new();
    };
    let query = mention::active_mention(state.composer.lines(), composer_cursor(state))
        .map(|m| m.query)
        .unwrap_or_default();
    mention::fuzzy_filter(files, &query, mention::PICKER_LIMIT)
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// Fetch the workspace file list the first time the picker opens, then
/// cache it for the session. No-op once loaded, loading, or failed, and
/// while the picker is closed.
async fn ensure_files_loaded(
    state: &mut StructuredViewState,
    toast_deadline: &mut Option<Instant>,
) {
    if state.mention.is_none() || !matches!(state.file_index, FileIndex::Unloaded) {
        return;
    }
    state.file_index = FileIndex::Loading;
    match state.http.files(&state.session_id).await {
        Ok(resp) => {
            state.file_index = FileIndex::Loaded {
                files: resp.files,
                truncated: resp.truncated,
            };
        }
        Err(e) => {
            tracing::warn!(target: "acp.tui", "file list fetch failed: {e}");
            let msg = e.to_string();
            state.file_index = FileIndex::Failed(msg.clone());
            set_toast(
                state,
                toast_deadline,
                format!("file list failed: {msg}"),
                ToastKind::Error,
            );
        }
    }
}

/// Move the picker highlight, clamped to the filtered result count.
fn navigate_mention(state: &mut StructuredViewState, delta: i32) {
    let len = filtered_mention_files(state).len();
    let Some(session) = state.mention.as_mut() else {
        return;
    };
    if len == 0 {
        session.selected = 0;
        return;
    }
    let cur = session.selected.min(len - 1) as i64;
    let next = (cur + delta as i64).rem_euclid(len as i64);
    session.selected = next as usize;
}

/// Insert the highlighted file and close the picker.
fn accept_mention(state: &mut StructuredViewState) {
    let files = filtered_mention_files(state);
    let Some(session) = state.mention.as_ref() else {
        return;
    };
    let Some(path) = files.get(session.selected.min(files.len().saturating_sub(1))) else {
        // Nothing to insert (empty filter); just close.
        state.mention = None;
        return;
    };
    let path = path.clone();
    if let Some(m) = mention::active_mention(state.composer.lines(), composer_cursor(state)) {
        mention::apply_selection(&mut state.composer, &m, &path);
    }
    state.mention = None;
    state.dismissed_mention = None;
}

fn apply_scroll(state: &mut StructuredViewState, delta: i32) {
    if delta == i32::MIN {
        state.scroll_offset = 0;
    } else if delta == i32::MAX {
        state.scroll_offset = u16::MAX;
    } else if delta < 0 {
        state.scroll_offset = state.scroll_offset.saturating_sub((-delta) as u16);
    } else {
        state.scroll_offset = state.scroll_offset.saturating_add(delta as u16);
    }
}

fn set_toast(
    state: &mut StructuredViewState,
    deadline: &mut Option<Instant>,
    text: String,
    kind: ToastKind,
) {
    state.toast = Some(ToastBanner { text, kind });
    *deadline = Some(Instant::now() + TOAST_TTL);
}

/// POST one prompt to the daemon, taking the optimistic in-flight lock
/// for the round-trip. The lock stays set on success (the WS turn-start
/// echo clears it) so a rapid second Enter queues instead of double-
/// firing; it is released on failure since no turn began. Returns whether
/// the POST succeeded.
async fn send_prompt_now(
    state: &mut StructuredViewState,
    toast_deadline: &mut Option<Instant>,
    text: &str,
) -> bool {
    state.in_flight = true;
    match state.http.prompt(&state.session_id, text).await {
        Ok(()) => true,
        Err(e) => {
            state.in_flight = false;
            set_toast(
                state,
                toast_deadline,
                format!("send failed: {e}"),
                ToastKind::Error,
            );
            false
        }
    }
}

/// Drain the next queued batch if the agent is idle. The batch is removed
/// from the queue only after its POST succeeds, so a failed send leaves
/// the prompts in place to retry (via the next turn-end edge or an empty-
/// composer flush) instead of silently dropping them.
async fn maybe_drain(state: &mut StructuredViewState, toast_deadline: &mut Option<Instant>) {
    if state.is_busy() || state.queue.is_empty() {
        return;
    }
    let Some((text, count)) = state.queue.next_batch(state.drain_mode) else {
        return;
    };
    if send_prompt_now(state, toast_deadline, &text).await {
        state.queue.drop_front(count);
        // Keep an in-progress ArrowUp/ArrowDown browse pointing at the
        // right entry now that the front of the queue shifted.
        state.reconcile_recall_after_drain(count);
        let remaining = state.queue.len();
        let msg = if remaining == 0 {
            "queue drained".to_string()
        } else {
            format!("draining queue ({remaining} waiting)")
        };
        set_toast(state, toast_deadline, msg, ToastKind::Info);
    }
}

fn redraw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    theme: &Theme,
    state: &StructuredViewState,
) -> Result<()> {
    terminal.draw(|f| render::render(f, f.area(), theme, state))?;
    Ok(())
}

fn render_error_screen(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    _theme: &Theme,
    message: &str,
) -> Result<()> {
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
    let msg = message.to_string();
    terminal.draw(|f| {
        let area = f.area();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Acp · error ");
        let para = Paragraph::new(msg.clone())
            .block(block)
            .wrap(Wrap { trim: false });
        f.render_widget(para, area);
    })?;
    Ok(())
}

async fn wait_for_dismiss(event_stream: &mut EventStream) -> Result<()> {
    while let Some(evt) = event_stream.next().await {
        if let Ok(CrosstermEvent::Key(_)) = evt {
            return Ok(());
        }
    }
    Ok(())
}
