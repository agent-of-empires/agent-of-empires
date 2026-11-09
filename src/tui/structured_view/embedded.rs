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
use crate::acp::client::WsMessage;
use crate::daemon::DaemonEndpoint;
use crate::daemon::WsError;
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

/// Retained transcript reader, without interactive state or an agent connection.
pub struct RetainedView {
    session_id: String,
    source_profile: String,
    generation: u64,
    session_info: crate::acp::session_paths::SessionViewInfo,
    worker: Option<super::retained::worker::Worker>,
    result: Option<std::sync::Arc<super::retained::worker::Update>>,
    request: Option<super::retained::worker::Request>,
    epoch: u64,
    document_epoch: u64,
    next_read: Instant,
    message: String,
    projection_changed: bool,
    copy_result: Option<tokio::sync::oneshot::Receiver<std::io::Result<std::fs::File>>>,
    clipboard_result:
        Option<tokio::sync::oneshot::Receiver<std::io::Result<crate::tui::clipboard::CopyReport>>>,
    copy_message: Option<String>,
    pending_copy: Option<(String, super::retained::worker::CopyRange)>,
}

impl RetainedView {
    pub fn new(endpoint: Option<DaemonEndpoint>, inst: &crate::session::Instance) -> Self {
        let mut view = Self {
            session_id: inst.id.clone(),
            source_profile: inst.source_profile.clone(),
            generation: inst.lifecycle_generation,
            session_info: crate::acp::session_paths::SessionViewInfo {
                title: inst.title.clone(),
                tool: inst.tool.clone(),
                acp_agent: inst.agent_name.clone().filter(|name| !name.is_empty()),
                paths: crate::acp::session_paths::SessionPathRoots {
                    id: inst.id.clone(),
                    project_path: inst
                        .pre_trash_project_path
                        .clone()
                        .unwrap_or_else(|| inst.project_path.clone()),
                    main_repo_path: inst
                        .worktree_info
                        .as_ref()
                        .map(|info| info.main_repo_path.clone()),
                    workspace_repos: inst
                        .workspace_info
                        .iter()
                        .flat_map(|info| &info.repos)
                        .map(|repo| crate::acp::session_paths::WorkspaceRepoRoot {
                            name: repo.name.clone(),
                            source_path: repo.source_path.clone(),
                        })
                        .collect(),
                },
            },
            worker: None,
            result: None,
            request: None,
            epoch: 0,
            document_epoch: 0,
            next_read: Instant::now() + std::time::Duration::from_secs(2),
            message: "Daemon unavailable; retained transcript could not be read.".into(),
            projection_changed: false,
            copy_result: None,
            clipboard_result: None,
            copy_message: None,
            pending_copy: None,
        };
        if let Some(endpoint) = endpoint {
            if let Err(error) = view.connect(endpoint) {
                view.message = format!("Transcript read failed: {error}");
            }
        }
        view
    }

    fn connect(&mut self, endpoint: DaemonEndpoint) -> Result<()> {
        let http = crate::acp::client::HttpClient::new(endpoint)?;
        let worker = super::retained::worker::Worker::new(
            http,
            self.session_id.clone(),
            self.session_info.paths.clone(),
        );
        if let Some(request) = &self.request {
            worker
                .epoch
                .store(request.epoch, std::sync::atomic::Ordering::Relaxed);
            worker.requests.send_replace(Some(request.clone()));
        }
        self.worker = Some(worker);
        self.message = "Loading retained ACP transcript…".into();
        Ok(())
    }

    pub fn matches(&self, inst: &crate::session::Instance) -> bool {
        inst.is_trashed()
            && self.session_id == inst.id
            && self.source_profile == inst.source_profile
            && self.generation == inst.lifecycle_generation
    }

    pub(crate) fn copy_selection(
        &mut self,
        width: u16,
        selection: crate::tui::rich_text::window::Selection,
    ) -> bool {
        self.start_copy(super::retained::worker::CopyRange::Selection { width, selection })
    }
    pub(crate) fn copy_all(&mut self) -> bool {
        self.start_copy(super::retained::worker::CopyRange::All)
    }
    fn start_copy(&mut self, range: super::retained::worker::CopyRange) -> bool {
        let Some(snapshot) = self
            .result
            .as_deref()
            .and_then(|result| result.as_ref().ok())
        else {
            return false;
        };
        if self.worker.is_none() {
            return false;
        }
        if snapshot
            .window
            .as_ref()
            .is_none_or(|window| window.total == 0)
        {
            return false;
        }
        self.pending_copy = Some((snapshot.identity.clone(), range));
        self.copy_message = Some("Copy queued…".into());
        self.poll_copy();
        true
    }
    fn poll_copy(&mut self) -> bool {
        use tokio::sync::oneshot::error::TryRecvError;
        let mut changed = false;
        if let Some(mut receiver) = self.copy_result.take() {
            match receiver.try_recv() {
                Ok(Ok(file)) => {
                    let (reply, result) = tokio::sync::oneshot::channel();
                    self.clipboard_result = Some(result);
                    tokio::task::spawn_blocking(move || {
                        let _ = reply.send(crate::tui::clipboard::copy_file(file));
                    });
                    self.copy_message = Some("Writing clipboard content…".into());
                    changed = true;
                }
                Ok(Err(error)) => {
                    self.copy_message = Some(format!("Copy failed: {error}"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => self.copy_result = Some(receiver),
                Err(TryRecvError::Closed) => {
                    self.copy_message = Some("Copy cancelled.".into());
                    changed = true;
                }
            }
        }
        if let Some(mut receiver) = self.clipboard_result.take() {
            match receiver.try_recv() {
                Ok(Ok(report)) => {
                    self.copy_message = Some(if let Some(desktop) = report.desktop {
                        format!("Copied {} bytes via {desktop}.", report.bytes)
                    } else if report.osc52_emitted {
                        format!(
                            "Emitted {} bytes via OSC 52; terminal clipboard support required.",
                            report.bytes
                        )
                    } else {
                        "No clipboard output was accepted.".into()
                    });
                    changed = true;
                }
                Ok(Err(error)) => {
                    self.copy_message = Some(format!("Copy failed: {error}"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => self.clipboard_result = Some(receiver),
                Err(TryRecvError::Closed) => {
                    self.copy_message = Some("Clipboard output ended unexpectedly.".into());
                    changed = true;
                }
            }
        }
        if self.copy_result.is_none() && self.clipboard_result.is_none() {
            if let Some((identity, range)) = self.pending_copy.take() {
                let current = self
                    .result
                    .as_deref()
                    .and_then(|result| result.as_ref().ok())
                    .is_some_and(|snapshot| snapshot.identity == identity);
                if let Some(worker) = self.worker.as_ref().filter(|_| current) {
                    self.copy_result = Some(worker.copy(identity, range));
                    self.copy_message = Some("Preparing clipboard content…".into());
                } else {
                    self.copy_message = Some("Copy cancelled: retained history changed.".into());
                }
                changed = true;
            }
        }
        changed
    }
    pub fn tick(&mut self) -> bool {
        let copy_changed = self.poll_copy();
        if self.worker.is_none() {
            if Instant::now() < self.next_read {
                return copy_changed;
            }
            self.next_read = Instant::now() + std::time::Duration::from_secs(2);
            let result = crate::acp::client::discover()
                .map_err(anyhow::Error::from)
                .and_then(|endpoint| self.connect(endpoint));
            if let Err(error) = result {
                let message = format!("Transcript read failed: {error}");
                let changed = self.message != message;
                self.message = message;
                return changed || copy_changed;
            }
            return true;
        }
        let worker = self.worker.as_mut().unwrap();
        if !worker.results.has_changed().unwrap_or(true) {
            return copy_changed;
        }
        let Some(result) = worker.results.borrow_and_update().clone() else {
            return copy_changed;
        };
        let previous = self
            .result
            .as_deref()
            .and_then(|result| result.as_ref().ok());
        match result.as_ref() {
            Ok(snapshot) => {
                self.projection_changed |= previous.is_none_or(|old| {
                    old.identity != snapshot.identity || old.highest_seq != snapshot.highest_seq
                });
                self.message = if snapshot
                    .window
                    .as_ref()
                    .is_some_and(|window| window.total == 0)
                {
                    "No readable retained ACP content. Native history is not available through this reader; destroyed tmux scrollback cannot be recovered.".into()
                } else if snapshot.window.is_none() {
                    "Preparing retained transcript…".into()
                } else {
                    "Retained ACP transcript · scroll to read · drag to copy".into()
                };
                if snapshot.incomplete {
                    self.message = format!(
                        "Retained ACP history is incomplete; some events are unavailable. {}",
                        self.message
                    );
                }
            }
            Err(error) => {
                self.projection_changed = true;
                self.message = format!("Transcript read failed: {error}");
                self.worker = None;
                self.next_read = Instant::now() + std::time::Duration::from_secs(2);
            }
        }
        if result.is_ok() {
            self.result = Some(result);
        }
        true
    }

    fn window(&self) -> Option<&super::retained::worker::Window> {
        let request = self.request.as_ref()?;
        let snapshot = self.result.as_deref()?.as_ref().ok()?;
        snapshot.window.as_ref().filter(|window| {
            window.width == request.width.max(1) && window.document_epoch == request.document_epoch
        })
    }

    pub fn effective_offset(&self) -> Option<usize> {
        self.window()
            .filter(|window| {
                Some(window.epoch) == self.request.as_ref().map(|request| request.epoch)
            })
            .map(|window| window.offset)
    }
    pub(crate) fn cancel_copy(&mut self) {
        self.copy_result = None;
        self.pending_copy = None;
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        offset: usize,
        focused: bool,
    ) -> (render::TranscriptGeometry, bool) {
        use ratatui::{style::Style, widgets::Paragraph};
        let mut status = Vec::new();
        let preparing = self.worker.is_some()
            && self.result.is_some()
            && (self
                .window()
                .is_none_or(|window| window.width != area.width)
                || self
                    .request
                    .as_ref()
                    .is_some_and(|request| request.theme != *theme));
        let message = if preparing {
            "Preparing retained transcript…"
        } else {
            &self.message
        };
        render::wrap_line_into(
            ratatui::text::Line::from(message.to_owned()),
            area.width,
            &mut status,
        );
        let chunks = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(status.len().min(u16::MAX as usize) as u16),
            ratatui::layout::Constraint::Min(0),
        ])
        .split(area);
        let base = if focused {
            "Trash · read-only · Esc: sessions · y: copy all"
        } else {
            "Trash · read-only · Enter: read"
        };
        let header = self
            .copy_message
            .as_ref()
            .map(|message| format!("{base} · {message}"));
        frame.render_widget(
            Paragraph::new(header.as_deref().unwrap_or(base))
                .style(Style::default().fg(theme.hint)),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(theme.text)),
            chunks[1],
        );
        let text_area = chunks[2];
        let reflow = self
            .request
            .as_ref()
            .is_none_or(|request| request.width != text_area.width || request.theme != *theme);
        let changed = std::mem::take(&mut self.projection_changed) || reflow;
        if self.request.as_ref().is_none_or(|request| {
            request.width != text_area.width
                || request.height != text_area.height
                || request.theme != *theme
                || request.offset != offset
        }) {
            self.epoch = self
                .epoch
                .checked_add(1)
                .expect("retained viewport epoch exhausted");
            let theme_changed = self
                .request
                .as_ref()
                .is_none_or(|request| request.theme != *theme);
            if theme_changed {
                self.document_epoch = self
                    .document_epoch
                    .checked_add(1)
                    .expect("retained document epoch exhausted");
            }
            let anchor = if reflow && offset != 0 {
                self.window().and_then(|window| window.anchor)
            } else {
                None
            };
            let request = super::retained::worker::Request {
                epoch: self.epoch,
                document_epoch: self.document_epoch,
                theme: theme.clone(),
                width: text_area.width,
                height: text_area.height,
                offset,
                anchor,
            };
            if let Some(worker) = &self.worker {
                worker
                    .epoch
                    .store(request.epoch, std::sync::atomic::Ordering::Relaxed);
                worker.requests.send_replace(Some(request.clone()));
            }
            self.request = Some(request);
        }
        let window = self.window();
        if let Some(window) = window {
            for (row, line) in window
                .text
                .lines
                .iter()
                .take(usize::from(text_area.height.min(window.height)))
                .enumerate()
            {
                frame.buffer_mut().set_line(
                    text_area.x,
                    text_area.y.saturating_add(row as u16),
                    line,
                    text_area.width,
                );
            }
        }
        (
            render::TranscriptGeometry {
                text_area,
                first_line: window.map_or(0, |window| window.first),
                total_lines: window.map_or(0, |window| window.total),
            },
            changed,
        )
    }
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
        let endpoint = crate::daemon::DaemonEndpoint::new(
            "http://127.0.0.1:8080".into(),
            None,
            crate::daemon::Source::Env,
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

    /// Route pointer motion to the view's popups. Only an active view takes the
    /// mouse, as with clicks. True when a redraw is needed.
    pub fn handle_hover(&mut self, col: u16, row: u16) -> bool {
        self.active && super::handle_hover(&mut self.state, col, row)
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

#[cfg(test)]
mod retained_tests;
