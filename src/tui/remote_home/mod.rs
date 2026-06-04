//! Remote home screen for cross-machine structured view attach.
//!
//! Activated when `AOE_DAEMON_URL` is set at startup (or `--daemon-url`
//! is passed on the CLI). Fetches the daemon's session list via
//! `GET /api/sessions`, filters to structured view-mode sessions (the only
//! kind that's meaningful to drive cross-machine; tmux PTYs can't be
//! attached remotely without SSH'ing into the host first), and lets
//! the user open one with Enter.
//!
//! Local-only operations are absent rather than disabled: a remote
//! session can't be `tmux attach`-ed from this machine, can't run
//! `aoe stop`, can't have its files edited locally. The web dashboard
//! covers the long-tail of remote management; this view's only job is
//! to be a fast lane into the structured view transcript + composer for a
//! known remote session.

mod render;

use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode, KeyEventKind};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use serde::Deserialize;

use crate::acp::client::discovery::DaemonEndpoint;
use crate::acp::client::HttpClient;
use crate::tui::styles::Theme;

/// Subset of `/api/sessions`'s `SessionResponse` we need. `serde` skips
/// unknown fields by default; we capture only the columns the remote
/// picker renders, so server-side additions don't break clients.
#[derive(Debug, Clone, Deserialize)]
pub struct RemoteSession {
    pub id: String,
    pub title: String,
    pub project_path: String,
    #[serde(default)]
    pub status: String,
    /// How the remote session renders. Defaults to `terminal` so an older
    /// daemon's response (which omits the field) still deserialises.
    #[serde(default)]
    pub view: crate::session::View,
}

pub struct RemoteHomeState {
    pub endpoint: DaemonEndpoint,
    pub sessions: Vec<RemoteSession>,
    pub cursor: usize,
    pub status_text: Option<String>,
    pub last_error: Option<String>,
    pub loading: bool,
}

impl RemoteHomeState {
    pub fn new(endpoint: DaemonEndpoint) -> Self {
        Self {
            endpoint,
            sessions: Vec::new(),
            cursor: 0,
            status_text: None,
            last_error: None,
            loading: true,
        }
    }

    pub fn move_cursor(&mut self, delta: i32) {
        let len = self.sessions.len();
        if len == 0 {
            self.cursor = 0;
            return;
        }
        let cur = self.cursor as i32;
        let next = (cur + delta).rem_euclid(len as i32);
        self.cursor = next as usize;
    }
}

/// Set up alternate-screen terminal, run the remote home loop, tear it
/// down. Invoked from `tui::run` when `AOE_DAEMON_URL` is set.
pub async fn run_standalone(endpoint: DaemonEndpoint) -> Result<()> {
    use crossterm::event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    };
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    use std::io;
    use std::io::IsTerminal;

    if !io::stdin().is_terminal() {
        anyhow::bail!("stdin is not a terminal; `aoe` needs an interactive TTY");
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
    let theme = crate::tui::styles::load_theme_with_mode("empire", false);

    let result = run(&mut terminal, &mut event_stream, &theme, endpoint).await;

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

async fn run(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    event_stream: &mut EventStream,
    theme: &Theme,
    endpoint: DaemonEndpoint,
) -> Result<()> {
    let mut state = RemoteHomeState::new(endpoint);
    refresh(&mut state).await;
    terminal.draw(|f| render::render(f, f.area(), theme, &state))?;

    while let Some(evt) = event_stream.next().await {
        let Ok(evt) = evt else { return Ok(()) };
        let CrosstermEvent::Key(key) = evt else {
            continue;
        };
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
            KeyCode::Char('r') => {
                state.loading = true;
                state.status_text = Some("refreshing…".to_string());
                terminal.draw(|f| render::render(f, f.area(), theme, &state))?;
                refresh(&mut state).await;
            }
            KeyCode::Down | KeyCode::Char('j') => state.move_cursor(1),
            KeyCode::Up | KeyCode::Char('k') => state.move_cursor(-1),
            KeyCode::Enter => {
                if let Some(session) = state.sessions.get(state.cursor).cloned() {
                    // Hand off to the structured view. Local-only actions
                    // are out of scope by design; tmux PTYs, file edits,
                    // and the like aren't reachable on this machine.
                    let endpoint = state.endpoint.clone();
                    super::structured_view::run_for_endpoint(
                        terminal,
                        event_stream,
                        theme,
                        endpoint,
                        &session.id,
                    )
                    .await?;
                    terminal.clear()?;
                }
            }
            _ => {}
        }
        terminal.draw(|f| render::render(f, f.area(), theme, &state))?;
    }
    Ok(())
}

async fn refresh(state: &mut RemoteHomeState) {
    state.loading = true;
    state.last_error = None;
    let client = match HttpClient::new(state.endpoint.clone()) {
        Ok(c) => c,
        Err(e) => {
            state.loading = false;
            state.last_error = Some(format!("http client init failed: {e}"));
            return;
        }
    };
    match client.list_sessions::<RemoteSession>().await {
        Ok(sessions) => {
            // Only structured view sessions are meaningful here: tmux sessions
            // can't be attached from another machine without SSH.
            let mut list: Vec<RemoteSession> = sessions
                .into_iter()
                .filter(|s| s.view == crate::session::View::Structured)
                .collect();
            list.sort_by(|a, b| a.title.cmp(&b.title));
            if state.cursor >= list.len() {
                state.cursor = list.len().saturating_sub(1);
            }
            state.sessions = list;
            state.status_text = Some(format!("{} session(s)", state.sessions.len()));
        }
        Err(e) => {
            state.last_error = Some(format!("{e}"));
            state.status_text = None;
        }
    }
    state.loading = false;
}
