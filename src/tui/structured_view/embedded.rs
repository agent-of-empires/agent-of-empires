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
use crate::acp::client::{DaemonEndpoint, WsMessage};
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

/// Retained transcript reader. No WebSocket, composer dispatcher, or worker setup.
pub struct RetainedView {
    session_id: String,
    source_profile: String,
    generation: u64,
    state: Option<StructuredViewState>,
    load: Option<tokio::task::JoinHandle<()>>,
    result: tokio::sync::oneshot::Receiver<
        Result<(Vec<crate::acp::transcript::TranscriptRow>, bool), String>,
    >,
    message: String,
}

impl RetainedView {
    pub fn new(endpoint: Option<DaemonEndpoint>, inst: &crate::session::Instance) -> Self {
        let (tx, result) = tokio::sync::oneshot::channel();
        let mut view = Self {
            session_id: inst.id.clone(),
            source_profile: inst.source_profile.clone(),
            generation: inst.lifecycle_generation,
            state: None,
            load: None,
            result,
            message: "Daemon unavailable; retained transcript could not be read.".into(),
        };
        let Some(endpoint) = endpoint else {
            return view;
        };
        let http = match crate::acp::client::HttpClient::new(endpoint.clone()) {
            Ok(http) => http,
            Err(error) => {
                view.message = format!("Transcript read failed: {error}");
                return view;
            }
        };
        let mut state = StructuredViewState::new(inst.id.clone(), endpoint, http.clone(), None);
        state.transcript.session_title = Some(inst.title.clone());
        view.state = Some(state);
        view.message = "Loading retained ACP transcript…".into();
        let id = inst.id.clone();
        view.load = Some(tokio::spawn(async move {
            let rows = http
                .replay_retained_paged(&id, 0, 1000)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(rows);
        }));
        view
    }

    pub fn matches(&self, inst: &crate::session::Instance) -> bool {
        inst.is_trashed()
            && self.session_id == inst.id
            && self.source_profile == inst.source_profile
            && self.generation == inst.lifecycle_generation
    }

    pub fn tick(&mut self) -> bool {
        if self.load.is_none() {
            return false;
        }
        let result = match self.result.try_recv() {
            Ok(result) => result,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return false,
            Err(error) => Err(error.to_string()),
        };
        self.load = None;
        match result {
            Ok((rows, lost)) => {
                self.message = if rows.is_empty() {
                    "No readable retained ACP content. Native history is not available through this reader; destroyed tmux scrollback cannot be recovered.".into()
                } else {
                    "Retained ACP transcript · scroll to read · drag to copy".into()
                };
                if lost {
                    self.message = format!(
                        "Retained ACP history is incomplete; some events are unavailable. {}",
                        self.message,
                    );
                }
                if let Some(state) = self.state.as_mut() {
                    state.transcript.server_rows = rows;
                }
            }
            Err(error) => self.message = format!("Transcript read failed: {error}"),
        }
        true
    }

    pub fn selection_text(&self, width: u16) -> ratatui::text::Text<'static> {
        self.state
            .as_ref()
            .filter(|state| !state.transcript.server_rows.is_empty())
            .map_or_else(Default::default, |state| {
                render::wrapped_transcript(
                    state,
                    &Theme::default(),
                    width,
                    render::ToolOutputMode::Retained,
                )
            })
    }

    pub fn render(
        &self,
        frame: &mut Frame,
        area: Rect,
        theme: &Theme,
        offset: usize,
        focused: bool,
    ) -> render::TranscriptGeometry {
        use ratatui::{style::Style, widgets::Paragraph};
        let mut status = Vec::new();
        render::wrap_line_into(
            ratatui::text::Line::from(self.message.clone()),
            area.width,
            &mut status,
        );
        let status_rows = status.len().min(u16::MAX as usize) as u16;
        let chunks = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(status_rows),
            ratatui::layout::Constraint::Min(0),
        ])
        .split(area);
        frame.render_widget(
            Paragraph::new(if focused {
                "Trash · read-only · Esc: sessions"
            } else {
                "Trash · read-only · Enter: read"
            })
            .style(Style::default().fg(theme.hint)),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new(status).style(Style::default().fg(theme.text)),
            chunks[1],
        );
        let text_area = chunks[2];
        let mut text = self
            .state
            .as_ref()
            .filter(|state| !state.transcript.server_rows.is_empty())
            .map_or_else(Default::default, |state| {
                render::wrapped_transcript(
                    state,
                    theme,
                    text_area.width,
                    render::ToolOutputMode::Retained,
                )
            });
        let total = text.lines.len();
        let max = total.saturating_sub(text_area.height as usize);
        let first = max.saturating_sub(offset);
        text.lines.drain(..first);
        text.lines.truncate(text_area.height as usize);
        frame.render_widget(Paragraph::new(text), text_area);
        render::TranscriptGeometry {
            text_area,
            first_line: first,
            total_lines: total,
        }
    }
}

impl Drop for RetainedView {
    fn drop(&mut self) {
        if let Some(load) = self.load.take() {
            load.abort();
        }
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
        render::wrapped_transcript(
            &self.state,
            &crate::tui::styles::Theme::default(),
            width,
            render::ToolOutputMode::Compact,
        )
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
mod retained_tests {
    use super::*;
    use axum::{
        extract::{Path, State},
        http::StatusCode,
        routing::get,
        Router,
    };
    use std::{sync::Arc, time::Duration};

    #[test]
    fn trash_home_reaches_history_beyond_u16_lines() {
        let inst = crate::session::Instance::new("large retained", "/tmp");
        let endpoint = DaemonEndpoint::new(
            "http://127.0.0.1:1".into(),
            None,
            crate::acp::client::discovery::Source::Env,
        );
        let http = crate::acp::client::HttpClient::new(endpoint.clone()).unwrap();
        let mut state = StructuredViewState::new(inst.id.clone(), endpoint, http, None);
        let mut model = crate::acp::transcript::TranscriptModel::new();
        model.apply_event(
            1,
            &crate::acp::state::Event::AgentMessageChunk {
                text: format!(
                    "oldest retained text\n\n{}newest retained text",
                    "line\n\n".repeat(70_000)
                ),
                message_id: None,
            },
        );
        state.transcript.merge_server_rows(model.rows().to_vec());
        let mut view = RetainedView::new(None, &inst);
        view.state = Some(state);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 10)).unwrap();
        let mut first = usize::MAX;
        terminal
            .draw(|frame| {
                first = view
                    .render(frame, frame.area(), &Theme::default(), usize::MAX, false)
                    .first_line;
            })
            .unwrap();
        assert_eq!(first, 0, "Home must reach the oldest retained line");
    }

    #[test]
    fn retained_tool_output_is_readable_and_copyable_without_preview_limits() {
        use crate::acp::state::{Event, ToolCall};
        for kind in ["execute", "read", "edit", "delete", "unknown"] {
            for failed in [false, true] {
                let inst = crate::session::Instance::new("retained output", "/tmp");
                let endpoint = DaemonEndpoint::new(
                    "http://127.0.0.1:1".into(),
                    None,
                    crate::acp::client::discovery::Source::Env,
                );
                let http = crate::acp::client::HttpClient::new(endpoint.clone()).unwrap();
                let mut state = StructuredViewState::new(inst.id.clone(), endpoint, http, None);
                let mut model = crate::acp::transcript::TranscriptModel::new();
                let at = chrono::DateTime::from_timestamp(100, 0).unwrap();
                model.apply_event(
                    1,
                    &Event::ToolCallStarted {
                        tool_call: ToolCall {
                            id: "tool".into(),
                            name: "RetainedTool".into(),
                            kind: kind.into(),
                            args_preview: format!("{}\nARGUMENT-END", "argument ".repeat(40)),
                            started_at: at,
                            parent_tool_call_id: None,
                            memory_recall: None,
                            diffs: if kind == "edit" {
                                vec![crate::acp::state::DiffPreview {
                                    path: "/tmp/retained.txt".into(),
                                    old_text: Some("before".into()),
                                    new_text: Some("changed\n".repeat(30) + "DIFF-END"),
                                    created_at: at,
                                }]
                            } else {
                                Vec::new()
                            },
                        },
                    },
                );
                let output = (0..40)
                    .map(|i| format!("output-{i:02}\n"))
                    .collect::<String>()
                    + "OUTPUT-END";
                let blocks = if kind == "unknown" {
                    vec![
                        crate::acp::state::ToolOutputBlock::Text {
                            text: output.clone(),
                        },
                        crate::acp::state::ToolOutputBlock::Image {
                            mime_type: "image/png".into(),
                            data: Some("BINARY-NOT-TEXT".into()),
                            uri: None,
                        },
                    ]
                } else {
                    Vec::new()
                };
                model.apply_event(
                    2,
                    &Event::ToolCallCompleted {
                        tool_call_id: "tool".into(),
                        is_error: failed,
                        content: if kind == "unknown" {
                            "FALLBACK-NOT-DISPLAYED".into()
                        } else {
                            output
                        },
                        output: blocks,
                        completed_at: at,
                        async_subagent: false,
                    },
                );
                state.transcript.server_rows = model.into_rows();
                let mut view = RetainedView::new(None, &inst);
                view.state = Some(state);
                for width in [32, 80] {
                    let copied = view
                        .selection_text(width)
                        .lines
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n");
                    assert!(
                        copied.contains("ARGUMENT-END"),
                        "{kind}, failed={failed}: {copied}"
                    );
                    for i in 0..40 {
                        assert!(
                            copied.contains(&format!("output-{i:02}")),
                            "{kind}, failed={failed}: missing output-{i:02}"
                        );
                    }
                    assert!(copied.contains("OUTPUT-END"));
                    assert!(!copied.contains("press `o`"));
                    if kind == "edit" {
                        assert!(copied.contains("DIFF-END"));
                    }
                    if kind == "unknown" {
                        assert!(copied.contains("[image image/png]"));
                        assert!(!copied.contains("BINARY-NOT-TEXT"));
                        assert!(!copied.contains("FALLBACK-NOT-DISPLAYED"));
                    }
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 12))
                            .unwrap();
                    terminal
                        .draw(|frame| {
                            view.render(frame, frame.area(), &Theme::default(), 0, true);
                        })
                        .unwrap();
                    let painted: String = terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect();
                    assert!(
                        painted.contains("OUTPUT-END"),
                        "{kind}, failed={failed}: {painted}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn retained_empty_history_does_not_hide_known_loss() {
        for empty in [false, true] {
            for incomplete in [false, true] {
                let inst = crate::session::Instance::new("retained state", "/tmp");
                let mut model = crate::acp::transcript::TranscriptModel::new();
                if !empty {
                    model.apply_event(
                        1,
                        &crate::acp::state::Event::AgentMessageChunk {
                            text: "retained message".into(),
                            message_id: None,
                        },
                    );
                }
                let (tx, rx) = tokio::sync::oneshot::channel();
                let mut view = RetainedView::new(None, &inst);
                view.result = rx;
                view.load = Some(tokio::spawn(async {}));
                tx.send(Ok((model.into_rows(), incomplete))).unwrap();
                assert!(view.tick());
                for width in [32, 100] {
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30))
                            .unwrap();
                    terminal
                        .draw(|frame| {
                            view.render(frame, frame.area(), &Theme::default(), 0, false);
                        })
                        .unwrap();
                    let painted: String = terminal
                        .backend()
                        .buffer()
                        .content()
                        .iter()
                        .map(|cell| cell.symbol())
                        .collect();
                    assert_eq!(
                        painted.contains("incomplete"),
                        incomplete,
                        "empty={empty}, width={width}: {painted}"
                    );
                    assert_eq!(
                        painted.contains("No readable"),
                        empty,
                        "incomplete={incomplete}, width={width}: {painted}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn obsolete_read_is_cancelled_and_failure_does_not_become_empty_history() {
        let started = Arc::new(tokio::sync::Notify::new());
        let app =
            Router::new()
                .route(
                    "/api/sessions/{id}/acp/replay",
                    get(
                        |Path(id): Path<String>,
                         State(started): State<Arc<tokio::sync::Notify>>| async move {
                            if id == "obsolete" {
                                started.notify_one();
                                std::future::pending::<()>().await;
                            }
                            StatusCode::SERVICE_UNAVAILABLE
                        },
                    ),
                )
                .with_state(started.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = DaemonEndpoint::new(
            format!("http://{}", listener.local_addr().unwrap()),
            None,
            crate::acp::client::discovery::Source::Env,
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let mut inst = crate::session::Instance::new("retained", "/tmp");
        inst.id = "obsolete".into();
        inst.trashed_at = Some(chrono::Utc::now());
        let obsolete = RetainedView::new(Some(endpoint.clone()), &inst);
        tokio::time::timeout(Duration::from_secs(5), started.notified())
            .await
            .unwrap();
        let task = obsolete.load.as_ref().unwrap().abort_handle();
        drop(obsolete);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        inst.id = "failed".into();
        let mut failed = RetainedView::new(Some(endpoint), &inst);
        tokio::time::timeout(Duration::from_secs(5), failed.load.as_mut().unwrap())
            .await
            .unwrap()
            .unwrap();
        assert!(failed.tick());
        assert!(failed.message.contains("503"), "{}", failed.message);
        assert!(failed.selection_text(60).lines.is_empty());
        server.abort();
        let _ = server.await;
    }
}
