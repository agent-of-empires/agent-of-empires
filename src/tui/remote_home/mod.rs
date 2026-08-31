//! Remote home screen for cross-machine session visibility and structured attach.
//!
//! The daemon projects every canonical session plus connection-scoped attach
//! and context-resume information. Terminal rows stay visible but cannot be
//! opened by this ACP-only client.
mod render;

use std::collections::BTreeMap;
use std::io::Stdout;

use crate::acp::client::discovery::DaemonEndpoint;
use crate::acp::client::{ClientCapability, HttpClient};
use crate::plugin::ui_state::UiSnapshot;
use crate::server::api::sessions::{AttachAvailability, AttachTransport, SessionInteraction};
use crate::session::config::{resolve_theme_name, resolve_theme_palette_mode};
use crate::tui::styles::Theme;
use anyhow::Result;
use crossterm::event::{Event as CrosstermEvent, EventStream, KeyCode, KeyEventKind};
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

#[derive(serde::Deserialize)]
struct RemoteSessionWire {
    id: String,
    title: String,
    project_path: String,
    status: String,
}
#[derive(Debug, Clone)]
pub struct RemoteSession {
    pub id: String,
    pub title: String,
    pub project_path: String,
    pub status: String,
    pub interaction: SessionInteraction,
}

impl RemoteSession {
    fn can_open(&self) -> bool {
        matches!(
            self.interaction.attach,
            AttachAvailability::Available {
                transport: AttachTransport::AcpWebsocketV1
            }
        )
    }
}

pub struct RemoteHomeState {
    pub endpoint: DaemonEndpoint,
    client: HttpClient,
    pub sessions: Vec<RemoteSession>,
    pub cursor: usize,
    pub status_text: Option<String>,
    pub last_error: Option<String>,
    pub loading: bool,
    pub plugin_ui: UiSnapshot,
}

impl RemoteHomeState {
    pub fn new(endpoint: DaemonEndpoint) -> Result<Self> {
        let client = HttpClient::new(endpoint.clone())?;
        Ok(Self {
            endpoint,
            client,
            sessions: Vec::new(),
            cursor: 0,
            status_text: None,
            last_error: None,
            loading: true,
            plugin_ui: UiSnapshot::default(),
        })
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
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
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
    // Push the kitty enhancement stack so the remote picker and the
    // structured-view it hands off to see `Shift+Enter` as a distinct
    // KeyEvent (#2362). Best-effort like `TerminalGuard::enter`; the
    // `AOE_DAEMON_URL` flow never enters via `TerminalGuard`.
    #[cfg(unix)]
    let _ = execute!(
        stdout,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let mut event_stream = EventStream::new();
    let theme_name = resolve_theme_name();
    let palette_mode = resolve_theme_palette_mode();
    let theme = crate::tui::styles::load_theme_with_mode(&theme_name, palette_mode);

    let result = run(&mut terminal, &mut event_stream, &theme, endpoint).await;

    #[cfg(unix)]
    let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
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
    let mut state = RemoteHomeState::new(endpoint)?;
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
                if let Some(session) = state
                    .sessions
                    .get(state.cursor)
                    .filter(|session| session.can_open())
                    .cloned()
                {
                    let endpoint = state.endpoint.clone();
                    super::structured_view::run_for_endpoint(
                        terminal,
                        event_stream,
                        theme,
                        endpoint,
                        &session.id,
                    )
                    .await?;
                    // Use the shared helper, not `terminal.clear()`: the latter
                    // does an `ESC[6n` cursor read that races the live
                    // `EventStream` and can abort with "cursor position could
                    // not be read" (see `crate::tui::clear_terminal`).
                    crate::tui::clear_terminal(terminal)?;
                }
            }
            _ => {}
        }
        terminal.draw(|f| render::render(f, f.area(), theme, &state))?;
    }
    Ok(())
}

fn sessions_from_snapshot(
    (wire_sessions, mut interactions): (
        Vec<RemoteSessionWire>,
        BTreeMap<String, SessionInteraction>,
    ),
) -> Result<Vec<RemoteSession>, String> {
    let mut seen = std::collections::HashSet::new();
    let mut sessions = Vec::with_capacity(wire_sessions.len());
    for session in wire_sessions {
        if !seen.insert(session.id.clone()) {
            return Err(format!("duplicate session id: {}", session.id));
        }
        let interaction = interactions
            .remove(&session.id)
            .ok_or_else(|| format!("missing interaction for session {}", session.id))?;
        sessions.push(RemoteSession {
            id: session.id,
            title: session.title,
            project_path: session.project_path,
            status: session.status,
            interaction,
        });
    }
    Ok(sessions)
}

fn apply_session_result(state: &mut RemoteHomeState, result: Result<Vec<RemoteSession>, String>) {
    match result {
        Ok(sessions) => {
            if state.cursor >= sessions.len() {
                state.cursor = sessions.len().saturating_sub(1);
            }
            state.sessions = sessions;
            state.status_text = Some(format!("{} session(s)", state.sessions.len()));
        }
        Err(error) => {
            state.sessions.clear();
            state.cursor = 0;
            state.last_error = Some(error);
            state.status_text = None;
        }
    }
}
async fn refresh(state: &mut RemoteHomeState) {
    state.loading = true;
    state.last_error = None;
    let sessions = state
        .client
        .list_sessions_with_interactions::<RemoteSessionWire, SessionInteraction>(&[
            ClientCapability::AcpWebsocketV1,
        ])
        .await
        .map_err(|error| error.to_string())
        .and_then(sessions_from_snapshot);
    apply_session_result(state, sessions);

    state.plugin_ui = match state.client.plugin_ui_state().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            tracing::debug!(target: "tui.remote_home", "plugin ui-state fetch failed: {error}");
            UiSnapshot::default()
        }
    };
    state.loading = false;
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::discovery::Source;

    fn session(id: &str) -> RemoteSession {
        RemoteSession {
            id: id.to_string(),
            title: id.to_string(),
            project_path: format!("/tmp/{id}"),
            status: "Stopped".to_string(),
            interaction: SessionInteraction {
                context_resume: crate::server::api::sessions::ContextResumeAvailability::Available,
                attach: AttachAvailability::Available {
                    transport: AttachTransport::AcpWebsocketV1,
                },
            },
        }
    }
    fn wire(id: &str) -> RemoteSessionWire {
        RemoteSessionWire {
            id: id.to_string(),
            title: id.to_string(),
            project_path: format!("/tmp/{id}"),
            status: "Stopped".to_string(),
        }
    }

    fn state() -> RemoteHomeState {
        RemoteHomeState::new(DaemonEndpoint::new(
            "http://127.0.0.1:8080".to_string(),
            None,
            Source::Env,
        ))
        .unwrap()
    }
    #[test]
    fn snapshot_requires_interaction_for_every_session() {
        let error = sessions_from_snapshot((vec![wire("missing")], BTreeMap::new())).unwrap_err();
        assert_eq!(error, "missing interaction for session missing");
    }

    #[test]
    fn snapshot_failure_clears_stale_sessions_and_handoff() {
        let mut state = state();
        state.sessions = vec![session("stale")];
        state.cursor = 4;
        state.status_text = Some("stale status".to_string());

        apply_session_result(&mut state, Err("missing interaction".to_string()));

        assert!(state.sessions.is_empty());
        assert_eq!(state.cursor, 0);
        assert_eq!(state.last_error.as_deref(), Some("missing interaction"));
        assert!(state.status_text.is_none());
    }

    #[test]
    fn successful_snapshot_preserves_server_order_and_ignores_extra_keys() {
        let mut interactions = BTreeMap::new();
        interactions.insert("second".to_string(), session("second").interaction);
        interactions.insert("first".to_string(), session("first").interaction);
        interactions.insert("extra".to_string(), session("extra").interaction);
        let sessions =
            sessions_from_snapshot((vec![wire("second"), wire("first")], interactions)).unwrap();

        let mut state = state();
        apply_session_result(&mut state, Ok(sessions));
        assert_eq!(
            state
                .sessions
                .iter()
                .map(|session| session.id.as_str())
                .collect::<Vec<_>>(),
            ["second", "first"]
        );
    }
}
