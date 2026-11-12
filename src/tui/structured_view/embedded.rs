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
    consecutive_failures: u32,
    message: String,
    projection_changed: bool,
    copy_result: Option<tokio::sync::oneshot::Receiver<std::io::Result<std::fs::File>>>,
    copy_ticket: Option<crate::tui::clipboard::Ticket>,
    clipboard_result:
        Option<tokio::sync::oneshot::Receiver<std::io::Result<crate::tui::clipboard::CopyReport>>>,
    copy_message: Option<String>,
    pending_copy: Option<(String, super::retained::worker::CopyRange)>,
}

pub(crate) struct RetainedPaint(std::sync::Weak<super::retained::worker::Update>);

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
            consecutive_failures: 0,
            message: "Daemon unavailable; retained transcript could not be read.".into(),
            projection_changed: false,
            copy_result: None,
            copy_ticket: None,
            clipboard_result: None,
            copy_message: None,
            pending_copy: None,
        };
        if let Some(endpoint) = endpoint {
            if let Err(error) = view.connect(endpoint) {
                view.message = format!("Transcript read failed: {error}");
                view.delay_after_failure();
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

    pub(crate) fn paint_token(&self) -> Option<RetainedPaint> {
        self.result
            .as_ref()
            .map(|result| RetainedPaint(std::sync::Arc::downgrade(result)))
    }

    pub(crate) fn matches_paint(&self, paint: &RetainedPaint) -> bool {
        self.result
            .as_ref()
            .is_some_and(|result| paint.0.ptr_eq(&std::sync::Arc::downgrade(result)))
    }

    #[cfg(test)]
    pub(crate) async fn wait_for_copy_preparation_for_test(&mut self) {
        let receiver = self.copy_result.take().expect("copy preparation requested");
        let prepared = tokio::time::timeout(std::time::Duration::from_secs(5), receiver)
            .await
            .expect("copy preparation deadline")
            .expect("copy preparer replied");
        let (sender, receiver) = tokio::sync::oneshot::channel();
        sender.send(prepared).unwrap();
        self.copy_result = Some(receiver);
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
            self.copy_message = Some(
                "Copy unavailable while retained content loads; retry after it appears.".into(),
            );
            return false;
        };
        if self.worker.is_none() {
            self.copy_message = Some(
                "Copy unavailable while retained reader reconnects; retry after content loads."
                    .into(),
            );
            return false;
        }
        if snapshot
            .window
            .as_ref()
            .is_none_or(|window| window.total == 0)
        {
            self.copy_message =
                Some("No readable retained content to copy yet; retry after content loads.".into());
            return false;
        }
        let identity = snapshot.identity.clone();
        let ticket = match crate::tui::clipboard::reserve() {
            Ok(ticket) => ticket,
            Err(error) => {
                self.copy_message = Some(format!("Copy failed: {error}"));
                return false;
            }
        };
        self.cancel_copy();
        self.copy_ticket = Some(ticket);
        self.pending_copy = Some((identity, range));
        self.copy_message = Some("Copy queued…".into());
        self.poll_copy();
        true
    }
    fn poll_copy(&mut self) -> bool {
        use tokio::sync::oneshot::error::TryRecvError;
        if self
            .copy_ticket
            .as_ref()
            .is_some_and(|ticket| !ticket.current())
        {
            self.cancel_copy();
            self.copy_message = Some("Copy superseded by a newer request.".into());
            return true;
        }
        let mut changed = false;
        if let Some(mut receiver) = self.copy_result.take() {
            match receiver.try_recv() {
                Ok(Ok(file)) => {
                    let submitted = self
                        .copy_ticket
                        .as_ref()
                        .filter(|ticket| ticket.current())
                        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::Interrupted))
                        .and_then(|ticket| {
                            let worker = self.worker.as_ref().ok_or_else(|| {
                                std::io::Error::from(std::io::ErrorKind::Interrupted)
                            })?;
                            let latest = worker.results.borrow();
                            let current = latest.as_deref().is_some_and(|latest| {
                                latest.as_ref().is_ok_and(|latest| {
                                    self.result.as_deref().is_some_and(|result| {
                                        result
                                            .as_ref()
                                            .is_ok_and(|shown| shown.identity == latest.identity)
                                    })
                                })
                            });
                            if !current {
                                return Err(std::io::ErrorKind::Interrupted.into());
                            }
                            ticket.submit(file, std::time::Duration::from_secs(30))
                        });
                    match submitted {
                        Ok(result) => {
                            self.clipboard_result = Some(result);
                            self.copy_message = Some("Writing clipboard content…".into());
                        }
                        Err(error) => {
                            self.copy_ticket = None;
                            self.copy_message =
                                Some(if error.kind() == std::io::ErrorKind::Interrupted {
                                    "Copy cancelled before output admission.".into()
                                } else {
                                    format!("Copy failed: {error}")
                                });
                        }
                    }
                    changed = true;
                }
                Ok(Err(error)) => {
                    self.copy_ticket = None;
                    self.copy_message = Some(format!("Copy failed: {error}"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => self.copy_result = Some(receiver),
                Err(TryRecvError::Closed) => {
                    self.copy_ticket = None;
                    self.copy_message = Some("Copy cancelled.".into());
                    changed = true;
                }
            }
        }
        if let Some(mut receiver) = self.clipboard_result.take() {
            match receiver.try_recv() {
                Ok(Ok(report)) => {
                    let current = self
                        .copy_ticket
                        .take()
                        .is_some_and(|ticket| ticket.current());
                    self.copy_message = Some(if current {
                        report.message()
                    } else {
                        "Copy superseded by a newer request.".into()
                    });
                    changed = true;
                }
                Ok(Err(error)) => {
                    self.copy_ticket = None;
                    self.copy_message = Some(format!("Copy failed: {error}"));
                    changed = true;
                }
                Err(TryRecvError::Empty) => self.clipboard_result = Some(receiver),
                Err(TryRecvError::Closed) => {
                    self.copy_ticket = None;
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
                    self.copy_ticket = None;
                    self.copy_message = Some("Copy cancelled: retained history changed.".into());
                }
                changed = true;
            }
        }
        changed
    }
    fn delay_after_failure(&mut self) {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let seconds = 2u64
            .saturating_mul(1u64 << self.consecutive_failures.saturating_sub(1).min(5))
            .min(60);
        self.next_read = Instant::now() + std::time::Duration::from_secs(seconds);
    }
    pub fn tick(&mut self) -> bool {
        let source_changed = self.poll_source();
        self.poll_copy() || source_changed
    }

    fn poll_source(&mut self) -> bool {
        if self.worker.is_none() {
            if Instant::now() < self.next_read {
                return false;
            }
            self.next_read = Instant::now() + std::time::Duration::from_secs(2);
            let result = crate::acp::client::discover()
                .map_err(anyhow::Error::from)
                .and_then(|endpoint| self.connect(endpoint));
            if let Err(error) = result {
                let message = format!("Transcript read failed: {error}");
                let changed = self.message != message;
                self.message = message;
                self.delay_after_failure();
                return changed;
            }
            return true;
        }
        let worker = self.worker.as_mut().unwrap();
        if !worker.results.has_changed().unwrap_or(true) {
            return false;
        }
        let Some(result) = worker.results.borrow_and_update().clone() else {
            return false;
        };
        let previous = self
            .result
            .as_deref()
            .and_then(|result| result.as_ref().ok());
        let preserve_window = previous.is_some_and(|snapshot| snapshot.window.is_some())
            && result
                .as_ref()
                .as_ref()
                .is_ok_and(|snapshot| snapshot.window.is_none());
        let projection_changed = previous.is_none_or(|old| match result.as_ref() {
            Ok(new) => old.identity != new.identity || old.highest_seq != new.highest_seq,
            Err(_) => true,
        });
        if projection_changed
            && (self.pending_copy.is_some()
                || self.copy_result.is_some()
                || self.clipboard_result.is_some())
        {
            self.cancel_copy();
            self.copy_message = Some("Copy cancelled: retained history changed.".into());
        }
        match result.as_ref() {
            Ok(snapshot) => {
                if snapshot.window.is_some() {
                    self.consecutive_failures = 0;
                }
                self.projection_changed |= projection_changed;
                self.message = if snapshot
                    .window
                    .as_ref()
                    .is_some_and(|window| window.total == 0)
                {
                    "No readable retained ACP content. Native history is not available through this reader; destroyed tmux scrollback cannot be recovered.".into()
                } else if preserve_window {
                    "Preparing replacement retained transcript; showing previous content…".into()
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
                self.delay_after_failure();
            }
        }
        if result.is_ok() && !preserve_window {
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
    /// Clearing geometry can cancel preparation, but an admitted output owns its receipt.
    pub(crate) fn cancel_selection_copy(&mut self) {
        if self.clipboard_result.is_some() {
            return;
        }
        let preparing = self.copy_result.is_some() || self.pending_copy.is_some();
        self.cancel_copy();
        if preparing {
            self.copy_message = Some("Copy cancelled before output admission.".into());
        }
    }

    fn cancel_copy(&mut self) {
        self.copy_ticket = None;
        self.copy_result = None;
        self.clipboard_result = None;
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
                text_area: Rect {
                    height: window.map_or(0, |window| text_area.height.min(window.height)),
                    ..text_area
                },
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
    pub(crate) fn interaction_revision(&self) -> u64 {
        self.state.interaction_revision
    }

    pub(crate) fn confirm_presentation(&mut self, revision: u64) -> bool {
        self.state.confirm_presentation(revision)
    }

    pub(crate) fn invalidate_presentation(&mut self) {
        self.state.presentation_ready = false;
    }

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

    /// Mounted fixture with controlled HTTP and caller-fed WebSocket messages.
    /// No automatic connection, worker startup or side-channel tasks.
    #[cfg(test)]
    pub(crate) fn for_test(session_id: &str, endpoint: Option<DaemonEndpoint>) -> Self {
        let endpoint = endpoint.unwrap_or_else(|| {
            crate::daemon::DaemonEndpoint::new(
                "http://127.0.0.1:8080".into(),
                None,
                crate::daemon::Source::Env,
            )
        });
        let http =
            crate::acp::client::HttpClient::new(endpoint.clone()).expect("test endpoint client");
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
        self.state.interaction_changed();
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
        self.state.interaction_changed();
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
        self.state.reconcile_interaction_authority();
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
        self.state.reconcile_interaction_authority();
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
        self.state.interaction_changed();
        super::paste_into_composer(&mut self.state, text);
        super::ensure_files_loaded(&mut self.state, &mut self.toast_deadline).await;
    }
}

#[cfg(test)]
mod retained_tests;
