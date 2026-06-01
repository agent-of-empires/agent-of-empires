//! Owned state for an open cockpit view: the focus, the reducer-
//! produced transcript, the composer text, and the websocket handle.
//! All side-effects (HTTP requests, browser opens, focus changes)
//! happen from [`super::mod`]'s async loop; this struct stays a plain
//! POD so the render layer can borrow it freely.

use ratatui_textarea::TextArea;

use super::input::Focus;
use super::queue::PromptQueue;
use super::reducer::CockpitTranscript;
use crate::cockpit::client::{DaemonEndpoint, HttpClient, WsHandle};
use crate::session::config::QueueDrainMode;

pub struct CockpitViewState {
    pub session_id: String,
    pub endpoint: DaemonEndpoint,
    pub http: HttpClient,
    pub transcript: CockpitTranscript,
    pub composer: TextArea<'static>,
    pub focus: Focus,
    pub scroll_offset: u16,
    /// Index into `transcript.pending_approvals` for the highlighted
    /// approval card when focus is `Approval`. None when the list is
    /// empty.
    pub selected_approval: Option<usize>,
    pub ws: Option<WsHandle>,
    /// Toast banner that appears briefly above the composer, e.g.
    /// "prompt sent" or an HTTP error.
    pub toast: Option<ToastBanner>,
    /// Prompts the user queued while a turn was in flight, awaiting the
    /// next idle drain. Pure local state, like the web composer's queue.
    pub queue: PromptQueue,
    /// How the queue drains on turn-end, resolved from the daemon's
    /// `/api/about` at startup (the TUI can attach to a remote daemon, so
    /// local config is not authoritative). Falls back to the config
    /// default if that fetch fails.
    pub drain_mode: QueueDrainMode,
    /// Optimistic in-flight lock: set the instant a prompt POST is sent
    /// and cleared when the daemon echoes the turn start / end (or the
    /// POST fails). Without it, a second Enter pressed in the window
    /// between the POST returning and the `UserPromptSent` echo would see
    /// a stale-idle reducer and fire a duplicate concurrent prompt.
    pub in_flight: bool,
}

#[derive(Debug, Clone)]
pub struct ToastBanner {
    pub text: String,
    pub kind: ToastKind,
}

#[derive(Debug, Clone, Copy)]
pub enum ToastKind {
    Info,
    Error,
}

impl CockpitViewState {
    pub fn new(
        session_id: String,
        endpoint: DaemonEndpoint,
        http: HttpClient,
        ws: Option<WsHandle>,
    ) -> Self {
        let mut composer = TextArea::default();
        composer.set_placeholder_text(" Message the agent…");
        composer.set_cursor_line_style(ratatui::style::Style::default());
        Self {
            transcript: CockpitTranscript::new(session_id.clone()),
            session_id,
            endpoint,
            http,
            composer,
            focus: Focus::Transcript,
            scroll_offset: u16::MAX, // stick to bottom by default; render clamps to last row
            selected_approval: None,
            ws,
            toast: None,
            queue: PromptQueue::default(),
            drain_mode: QueueDrainMode::default(),
            in_flight: false,
        }
    }

    /// Whether a fresh Enter should park in the queue rather than send
    /// now. Busy when the agent is mid-turn, a POST is in flight, or the
    /// WebSocket is down (no handle): in every case an immediate send
    /// would either collide with the running turn or fire into a daemon
    /// whose turn boundaries we can no longer observe.
    pub fn is_busy(&self) -> bool {
        self.transcript.turn_active || self.in_flight || self.ws.is_none()
    }

    /// Drain the composer's current text and clear it so the user can
    /// start the next prompt.
    pub fn take_composer_text(&mut self) -> String {
        let text = self.composer.lines().join("\n").trim().to_string();
        // Replace with a fresh textarea so cursor + selection state
        // also reset; ratatui-textarea has no public "clear" today.
        let mut next = TextArea::default();
        next.set_placeholder_text(" Message the agent…");
        next.set_cursor_line_style(ratatui::style::Style::default());
        self.composer = next;
        text
    }

    /// Bring the selected-approval index back into bounds whenever the
    /// pending list changes underneath us (a resolution removed one,
    /// a new request added one, etc.).
    pub fn reconcile_selection(&mut self) {
        let len = self.transcript.pending_approvals.len();
        if len == 0 {
            self.selected_approval = None;
            if matches!(self.focus, Focus::Approval) {
                self.focus = Focus::Transcript;
            }
            return;
        }
        match self.selected_approval {
            Some(i) if i >= len => self.selected_approval = Some(len - 1),
            None => self.selected_approval = Some(0),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cockpit::client::discovery::Source;

    fn test_state(ws: Option<WsHandle>) -> CockpitViewState {
        let endpoint = DaemonEndpoint {
            base_url: "http://127.0.0.1:8080".into(),
            token: None,
            source: Source::Env,
        };
        let http = HttpClient::new(endpoint.clone()).unwrap();
        CockpitViewState::new("s-1".into(), endpoint, http, ws)
    }

    #[test]
    fn fresh_state_has_idle_turn_flags() {
        let state = test_state(None);
        assert!(!state.transcript.turn_active);
        assert!(!state.in_flight);
    }

    #[test]
    fn busy_while_turn_active() {
        let mut state = test_state(None);
        state.transcript.turn_active = true;
        assert!(state.is_busy());
    }

    #[test]
    fn busy_while_post_in_flight() {
        let mut state = test_state(None);
        state.in_flight = true;
        assert!(state.is_busy());
    }

    #[test]
    fn busy_while_socket_down() {
        // A dropped WebSocket (ws = None) must force queuing, since turn
        // boundaries can't be observed to drive an immediate send.
        let state = test_state(None);
        assert!(state.is_busy());
    }

    #[test]
    fn enqueue_grows_the_local_queue() {
        let mut state = test_state(None);
        assert!(state.queue.is_empty());
        state.queue.push("hello".into());
        state.queue.push("world".into());
        assert_eq!(state.queue.len(), 2);
        let items: Vec<&String> = state.queue.iter().collect();
        assert_eq!(items, vec!["hello", "world"]);
    }
}
