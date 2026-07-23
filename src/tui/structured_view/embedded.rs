//! Embedded (preview-pane) variant of the structured view.
//!
//! The full-screen loop in the parent module owns the terminal and the
//! event stream for the duration of the view. The embedded variant
//! instead lives inside the home screen's `App` loop, rendering into
//! the preview pane while the session list stays visible, mirroring
//! how live-send drives a terminal agent without leaving the home view.
//!
//! Split of responsibilities with the `App` loop:
//! - [`EmbeddedView::next_event`] is the cancel-safe await the App's
//!   `tokio::select!` races against its other arms. It only ever awaits
//!   channel receives, so dropping the future mid-poll loses nothing.
//! - [`EmbeddedView::apply_event`] runs in the winning arm's body
//!   (never cancelled) and may perform HTTP work: replay rehydration,
//!   queue drains, the bounded-backoff reconnect.
//! - Terminal input routes through [`EmbeddedView::handle_event`],
//!   which shares the parent module's intent dispatcher, so keybindings
//!   cannot drift from the full-screen view.

use anyhow::Result;
use crossterm::event::Event as CrosstermEvent;
use ratatui::layout::Rect;
use ratatui::Frame;
use tokio::time::Instant;

use super::state::{StructuredViewState, ToastKind};
use super::{
    apply_ws_message, drain_plugin_toast, handle_terminal_event, render, set_toast, setup_view,
    PluginPoll, ViewSetup,
};
use crate::acp::client::{DaemonEndpoint, WsError, WsMessage};
use crate::acp::session_paths::SessionViewInfo;
use crate::tui::styles::Theme;

/// One event surfaced by [`EmbeddedView::next_event`], applied by
/// [`EmbeddedView::apply_event`]. The two-phase shape exists for
/// cancellation safety; see the module docs.
pub enum EmbeddedEvent {
    /// A WebSocket message, or `None` when the ws channel closed.
    Ws(Option<Result<WsMessage, WsError>>),
    Plugin(PluginPoll),
    SessionInfo(Result<SessionViewInfo, String>),
}

pub struct EmbeddedView {
    state: StructuredViewState,
    toast_deadline: Option<Instant>,
    plugin_rx: tokio::sync::mpsc::Receiver<PluginPoll>,
    session_info_rx: tokio::sync::mpsc::Receiver<Result<SessionViewInfo, String>>,
    /// Preview vs. interactive. A view is mounted (streaming, rendered
    /// in the preview pane) as soon as its session is selected, but the
    /// keyboard only routes to it once activated (Enter), the same
    /// preview-then-enter model terminal sessions use for live-send.
    active: bool,
}

impl EmbeddedView {
    /// Connect to `session_id` on an already-located daemon: hydrate
    /// the transcript, open the WebSocket, spawn the side-channel
    /// tasks. Startup errors (replay/ws) surface as a toast rather
    /// than a hard failure, matching the full-screen view. Starts in
    /// preview (inactive) state.
    pub async fn connect(endpoint: DaemonEndpoint, session_id: &str) -> Result<Self> {
        let ViewSetup {
            state,
            startup_toast,
            plugin_rx,
            session_info_rx,
        } = setup_view(endpoint, session_id).await?;
        let mut view = Self {
            state,
            toast_deadline: None,
            plugin_rx,
            session_info_rx,
            active: false,
        };
        if let Some(text) = startup_toast {
            set_toast(
                &mut view.state,
                &mut view.toast_deadline,
                text,
                ToastKind::Error,
            );
        }
        // A question already pending in the replay presents its menu now.
        super::auto_present_elicitation(&mut view.state, &mut view.toast_deadline);
        Ok(view)
    }

    /// The session this view is streaming.
    pub fn session_id(&self) -> &str {
        &self.state.session_id
    }

    /// Whether the keyboard is routed to this view (interactive) rather
    /// than the home list (preview).
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Enter interactive mode: the composer takes the keyboard and the
    /// caret shows. Focus returns to the composer so typing works at
    /// once (a pending approval re-grabs it on the next reconcile).
    pub fn activate(&mut self) {
        self.active = true;
        if matches!(self.state.focus, super::input::Focus::Transcript) {
            self.state.focus = super::input::Focus::Composer;
        }
    }

    /// Leave interactive mode back to a read-only preview (Ctrl+Q). The
    /// view stays mounted and streaming.
    pub fn deactivate(&mut self) {
        self.active = false;
    }

    /// Await the next daemon-side event. Cancel-safe: only channel
    /// receives are awaited, so the App loop may freely race this
    /// against terminal input and drop the losing future. With no live
    /// WebSocket the ws arm pends forever and only the side channels
    /// can wake us, mirroring the full-screen loop's do-not-spin
    /// behavior after a failed reconnect.
    pub async fn next_event(&mut self) -> EmbeddedEvent {
        let ws = self.state.ws.as_mut();
        tokio::select! {
            msg = async {
                match ws {
                    Some(handle) => handle.recv().await,
                    None => std::future::pending().await,
                }
            } => EmbeddedEvent::Ws(msg),
            Some(poll) = self.plugin_rx.recv() => EmbeddedEvent::Plugin(poll),
            Some(result) = self.session_info_rx.recv() => EmbeddedEvent::SessionInfo(result),
        }
    }

    /// Apply one event from [`next_event`]. May perform HTTP work
    /// (replay, drain, reconnect); the caller must not race this
    /// against other futures.
    pub async fn apply_event(&mut self, event: EmbeddedEvent) {
        match event {
            EmbeddedEvent::Ws(Some(msg)) => {
                apply_ws_message(&mut self.state, &mut self.toast_deadline, msg).await;
            }
            EmbeddedEvent::Ws(None) => {
                // Channel closed without an error frame: treat as a
                // disconnect so `next_event` stops polling the dead
                // handle and is_busy() queues new prompts.
                self.state.ws = None;
                self.state.in_flight = false;
                set_toast(
                    &mut self.state,
                    &mut self.toast_deadline,
                    "ws closed".into(),
                    ToastKind::Error,
                );
            }
            EmbeddedEvent::Plugin(poll) => {
                if let Some(commands) = poll.commands {
                    self.state.plugin_commands = commands;
                }
                self.state.ingest_plugin_ui(poll.snapshot);
                drain_plugin_toast(&mut self.state, &mut self.toast_deadline);
            }
            EmbeddedEvent::SessionInfo(result) => match result {
                Ok(info) => super::apply_session_info(&mut self.state, info),
                Err(e) => {
                    tracing::warn!(
                        target: "acp.tui",
                        "session info fetch failed; rendering fallback header and raw paths: {e}"
                    );
                }
            },
        }
    }

    /// Route one terminal event (key / paste / mouse) through the
    /// shared intent dispatcher. Returns `true` when the user asked to
    /// exit the view (Esc from the transcript).
    pub async fn handle_event(&mut self, evt: CrosstermEvent) -> Result<bool> {
        handle_terminal_event(&mut self.state, evt, &mut self.toast_deadline).await
    }

    /// Periodic housekeeping driven by the App's refresh ticker:
    /// expire the toast and surface the next queued plugin
    /// notification. Returns `true` when something visible changed.
    pub fn tick(&mut self) -> bool {
        let mut changed = false;
        if let Some(deadline) = self.toast_deadline {
            if Instant::now() >= deadline {
                self.state.toast = None;
                self.toast_deadline = None;
                changed = true;
            }
        }
        let had_toast = self.state.toast.is_some();
        drain_plugin_toast(&mut self.state, &mut self.toast_deadline);
        changed || (self.state.toast.is_some() != had_toast)
    }

    /// Render into `area` (the home view's preview body). Also stashes
    /// the computed layout, in real frame coordinates, so subsequent
    /// mouse events hit-test against what is actually on screen.
    /// Returns the transcript geometry so the home view can point its
    /// drag-select machinery at the painted rows.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
    ) -> Option<render::TranscriptGeometry> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        // The home view may have painted placeholder content under us
        // this frame; the structured renderer assumes an empty buffer
        // (it grew up full-screen) and skips cells with no content, so
        // reset the area first or stale text shows through.
        frame.render_widget(ratatui::widgets::Clear, area);
        self.state.layout = Some(render::compute_layout(area, &self.state));
        Some(render::render(frame, area, theme, &self.state, self.active))
    }

    /// The transcript as the exact pre-wrapped rows the last render
    /// painted at `width` columns. The home view's selection extraction
    /// slices these by (row, column), so they must match the on-screen
    /// geometry; sharing `wrapped_transcript` with the renderer
    /// guarantees it. Styles are irrelevant to extraction, so a default
    /// theme keeps this callable without one.
    pub fn selection_text(&self, width: u16) -> ratatui::text::Text<'static> {
        render::wrapped_transcript(&self.state, &crate::tui::styles::Theme::default(), width)
    }
}
