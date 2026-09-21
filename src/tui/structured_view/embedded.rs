//! Embedded (preview-pane) variant of the structured view.
//!
//! Unlike the full-screen loop in the parent module, this lives inside the home
//! screen's `App` loop, rendering into the preview pane while the session list
//! stays visible, the way live-send drives a terminal agent.
//!
//! [`EmbeddedView::next_event`] is the cancel-safe await the App's `select!`
//! races: it only awaits channel receives, so dropping the future loses nothing.
//! [`EmbeddedView::apply_event`] runs in the winning arm's body and may do HTTP
//! work (replay rehydration, queue drains, reconnect). Terminal input routes
//! through [`EmbeddedView::handle_event`], sharing the parent module's intent
//! dispatcher so keybindings cannot drift.
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
use crate::tui::styles::Theme;

/// One event surfaced by [`EmbeddedView::next_event`], applied by
/// [`EmbeddedView::apply_event`]. The two-phase shape exists for
/// cancellation safety; see the module docs.
pub enum EmbeddedEvent {
    /// A WebSocket message, or `None` when the ws channel closed.
    Ws(Option<Result<WsMessage, WsError>>),
    Plugin(PluginPoll),
    SessionInfo(super::ViewSideInfo),
}

pub struct EmbeddedView {
    state: StructuredViewState,
    toast_deadline: Option<Instant>,
    plugin_rx: tokio::sync::mpsc::Receiver<PluginPoll>,
    session_info_rx: tokio::sync::mpsc::Receiver<super::ViewSideInfo>,
    /// Preview vs. interactive. A view streams into the preview pane as soon as
    /// its session is selected, but the keyboard routes to it only once activated
    /// (Enter), the preview-then-enter model terminal sessions use.
    active: bool,
}

impl EmbeddedView {
    /// Connect to `session_id` on an already-located daemon: hydrate the
    /// transcript, open the WebSocket, spawn the side-channel tasks. Startup
    /// errors surface as a toast. Starts in preview (inactive) state.
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

    /// Test constructor: a mounted, non-activated view over a state that
    /// never talks to a daemon. Lets App-level tests drive the paste-drain
    /// handoff without a live connection.
    #[cfg(test)]
    pub(crate) fn for_test(session_id: &str) -> Self {
        let endpoint = crate::acp::client::DaemonEndpoint::new(
            "http://127.0.0.1:8080".into(),
            None,
            crate::acp::client::discovery::Source::Env,
        );
        let http =
            crate::acp::client::HttpClient::new(endpoint.clone()).expect("fake endpoint client");
        Self {
            state: crate::tui::structured_view::StructuredViewState::new(
                session_id.into(),
                endpoint,
                http,
                None,
            ),
            toast_deadline: None,
            plugin_rx: tokio::sync::mpsc::channel(1).1,
            session_info_rx: tokio::sync::mpsc::channel(1).1,
            active: false,
        }
    }

    /// Composer content, joined on newlines: test read for the paste-drain
    /// handoff.
    #[cfg(test)]
    pub(crate) fn composer_text(&self) -> String {
        self.state.composer.lines().join("\n")
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

    /// Enter interactive mode: the composer takes the keyboard and the caret
    /// shows. A pending approval re-grabs focus on the next reconcile.
    pub fn activate(&mut self) {
        self.active = true;
        if matches!(self.state.focus, super::input::Focus::Transcript) {
            self.state.focus = super::input::Focus::Composer;
        }
    }

    /// Leave interactive mode back to a read-only preview (Ctrl+Q). The
    /// view stays mounted and streaming.
    ///
    /// Drops the plugin pane overlay (#2467): it is modal and keyed off focus, so
    /// leaving it up would paint an unclosable panel over the home preview.
    pub fn deactivate(&mut self) {
        self.active = false;
        self.state.close_plugin_pane();
    }

    /// Await the next daemon-side event. Cancel-safe: only channel receives are
    /// awaited. With no live WebSocket that arm pends forever and only the side
    /// channels wake us, mirroring the full-screen loop.
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
            Some(side) = self.session_info_rx.recv() => EmbeddedEvent::SessionInfo(side),
        }
    }

    /// Apply an event from [`Self::next_event`]. May perform HTTP work
    /// (replay, drain, reconnect); do not race this against other futures.
    pub async fn apply_event(&mut self, event: EmbeddedEvent) {
        match event {
            EmbeddedEvent::Ws(Some(msg)) => {
                apply_ws_message(&mut self.state, &mut self.toast_deadline, msg).await;
            }
            EmbeddedEvent::Ws(None) => {
                // Channel closed without an error frame: treat as a disconnect so
                // `next_event` stops polling the dead handle.
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
            EmbeddedEvent::SessionInfo(side) => super::apply_side_info(&mut self.state, side),
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

    /// Render into `area` (the home view's preview body), stashing the computed
    /// layout in real frame coordinates so later mouse events hit-test against
    /// what is on screen. Returns the transcript geometry for drag-select.
    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
    ) -> Option<render::TranscriptGeometry> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        // The home view may have painted placeholder content this frame; the
        // structured renderer assumes an empty buffer and skips empty cells, so
        // reset the area or stale text shows through.
        frame.render_widget(ratatui::widgets::Clear, area);
        self.state.layout = Some(render::compute_layout(area, &self.state));
        Some(render::render(frame, area, theme, &self.state, self.active))
    }

    /// The transcript as the exact pre-wrapped rows the last render painted at
    /// `width` columns, so the home view's selection extraction slices match the
    /// on-screen geometry. Styles are irrelevant to extraction.
    pub fn selection_text(&self, width: u16) -> ratatui::text::Text<'static> {
        render::wrapped_transcript(&self.state, &crate::tui::styles::Theme::default(), width)
    }

    /// Paste text into the composer, focusing it if needed, then load the file
    /// index when the paste leaves an open `@`-mention, so a paste forwarded from
    /// the home view lands in the same state a direct one would.
    pub async fn paste_text_with_file_load(&mut self, text: &str) {
        super::paste_into_composer(&mut self.state, text);
        super::ensure_files_loaded(&mut self.state, &mut self.toast_deadline).await;
    }
}
