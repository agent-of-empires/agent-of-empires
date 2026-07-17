//! Owned state for an open structured view: the focus, the reducer-
//! produced transcript, the composer text, and the websocket handle.
//! All side-effects (HTTP requests, browser opens, focus changes)
//! happen from [`super::mod`]'s async loop; this struct stays a plain
//! POD so the render layer can borrow it freely.

use std::collections::VecDeque;

use ratatui::layout::Rect;
use ratatui_textarea::TextArea;

use super::input::Focus;
use super::queue::PromptQueue;
use super::reducer::AcpTranscript;
use super::slash;
use crate::acp::client::{DaemonEndpoint, HttpClient, WsHandle};
use crate::acp::session_paths::SessionPathRoots;
use crate::acp::state::AvailableCommand;
use crate::plugin::ui_state::{Notification, UiSnapshot};
use crate::tui::plugin_ui;

/// Most plugin notifications buffered locally awaiting a free toast slot. The
/// daemon already bounds its notification ring; this is a second guard so a
/// burst can't grow the view's memory, and old un-shown toasts drop rather
/// than the newest.
const MAX_PENDING_PLUGIN_TOASTS: usize = 20;

pub struct StructuredViewState {
    pub session_id: String,
    pub endpoint: DaemonEndpoint,
    pub http: HttpClient,
    pub transcript: AcpTranscript,
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
    /// Optimistic in-flight lock: set the instant a prompt POST is sent
    /// and cleared when the daemon echoes the turn start / end (or the
    /// POST fails). Without it, a second Enter pressed in the window
    /// between the POST returning and the `UserPromptSent` echo would see
    /// a stale-idle reducer and fire a duplicate concurrent prompt.
    pub in_flight: bool,
    /// Highlighted row in the slash-command picker. Meaningful only
    /// while the picker is open; clamped against the live match count.
    pub slash_selected: usize,
    /// The exact slash query the user dismissed with Esc. The picker
    /// stays closed while the composer text still maps to this query,
    /// so cursor movement (which the textarea reports as edits) can't
    /// reopen it; the picker reappears only once the query text
    /// actually changes.
    pub dismissed_slash_query: Option<String>,
    /// Workspace file list for the `@`-mention picker, fetched once per
    /// session on the first `@` and cached for its lifetime.
    pub file_index: FileIndex,
    /// Session roots used to shorten tool-call paths in the transcript.
    /// Fetched best-effort from the daemon; `None` keeps raw paths.
    pub path_roots: Option<SessionPathRoots>,
    /// `Some` while the `@`-mention picker is open. Holds only the
    /// highlighted-row index; the query and token range are always
    /// recomputed from the composer via [`super::mention::active_mention`]
    /// so there is a single source of truth for the typed text.
    pub mention: Option<MentionSession>,
    /// Anchor `(row, col)` of an `@`-token the user dismissed with Esc.
    /// Keeps the picker closed while they keep typing in that same
    /// token; cleared once the token goes away or a fresh `@` is typed.
    pub dismissed_mention: Option<(usize, usize)>,
    /// Active ArrowUp/ArrowDown queue-recall browse, or `None` when the
    /// composer is in its normal typing mode. See [`RecallState`].
    pub recall: Option<RecallState>,
    /// Latest plugin UI-state snapshot polled from the daemon (#2402). Empty
    /// until the first poll lands; the status line renders the
    /// TUI-applicable slots (global `StatusBar`, this session's
    /// `DetailBadge`) from it.
    pub plugin_ui: UiSnapshot,
    /// Notification bookkeeping so each `ui.notify` toasts once. See
    /// [`PluginNotifyState`].
    pub plugin_notify: PluginNotifyState,
    /// Pane rectangles of the most recent draw, so mouse events can be
    /// hit-tested against what is actually on screen. `None` until the
    /// first frame renders.
    pub layout: Option<ViewLayout>,
    /// Floating single-choice picker: the permission-mode picker
    /// (Shift+Tab), or the auto-opened answer menu for a pending
    /// single-select question. `None` when closed. While open it owns
    /// Up/Down/Enter/Esc, whatever the focus.
    pub choice: Option<ChoicePicker>,
    /// Nonce of the elicitation whose answer menu was last auto-opened,
    /// so a pending question presents itself once, not repeatedly, instead of
    /// re-opening (or re-toasting) on every frame.
    pub auto_presented_elicitation: Option<String>,
    /// The maximum scroll offset (wrapped rows minus visible rows) from
    /// the last render, stashed so a wheel/PageUp step can resolve the
    /// `u16::MAX` "stick to bottom" sentinel to a concrete position
    /// before moving. Interior-mutable so the render (which borrows the
    /// state immutably) can record it. See `apply_scroll`.
    pub last_scroll_max: std::cell::Cell<u16>,
}

/// One open choice-picker: a titled option list plus what accepting the
/// highlighted option means. Kept generic so the mode picker and the
/// elicitation answer flow share the input routing and render path.
pub struct ChoicePicker {
    pub title: String,
    /// `(value, label)` rows; `value` is what gets submitted.
    pub options: Vec<(String, String)>,
    pub selected: usize,
    pub purpose: ChoicePurpose,
}

pub enum ChoicePurpose {
    /// Accepting POSTs `session/set_mode` with the chosen mode id.
    Mode,
    /// Accepting records the answer for the current question and either
    /// advances to the next single-select question or, when `remaining`
    /// is empty, POSTs the accumulated answers.
    Elicitation {
        nonce: String,
        /// Field key the open picker answers.
        field_key: String,
        /// Single-select questions still to ask after this one.
        remaining: Vec<crate::acp::elicitations::ElicitationQuestion>,
        /// Answers accumulated so far, keyed by field key.
        answers: std::collections::BTreeMap<String, crate::acp::elicitations::AnswerValue>,
    },
}

impl ChoicePicker {
    /// Move the highlight by `delta`, wrapping at both ends.
    pub fn navigate(&mut self, delta: i32) {
        let len = self.options.len();
        if len == 0 {
            self.selected = 0;
            return;
        }
        let cur = self.selected.min(len - 1) as i64;
        self.selected = (cur + delta as i64).rem_euclid(len as i64) as usize;
    }
}

/// Where each pane landed in the last-rendered frame, in screen
/// coordinates. Computed by `render::compute_layout` and stored here on
/// every redraw; the input layer maps mouse clicks to focus regions
/// against it.
#[derive(Debug, Clone, Copy)]
pub struct ViewLayout {
    pub transcript: Rect,
    pub status: Rect,
    pub queue: Rect,
    pub composer: Rect,
}

/// Tracks which plugin notifications have been shown and buffers any awaiting
/// a free toast slot, so a poll that returns several new notifications shows
/// them in turn instead of clobbering the single-slot banner down to the last.
#[derive(Debug, Default)]
pub struct PluginNotifyState {
    /// Highest notification seq already accounted for. Notifications at or
    /// below this are not re-toasted.
    pub last_seen_seq: u64,
    /// False until the first snapshot establishes the baseline. The first
    /// snapshot only sets `last_seen_seq` (pre-existing notifications must not
    /// toast on open); subsequent ones enqueue what is genuinely new.
    pub initialized: bool,
    /// Notifications waiting to be shown, oldest first.
    pub pending: VecDeque<Notification>,
}

/// In-progress shell-history-style browse of the prompt queue. The user
/// pressed ArrowUp on an empty-origin composer; `index` points at the
/// queued entry currently loaded into the composer and `stashed_draft`
/// holds the text that was there before browsing started, restored when
/// ArrowDown walks back past the newest entry.
#[derive(Debug, Clone)]
pub struct RecallState {
    pub index: usize,
    pub stashed_draft: String,
}

/// Build a composer textarea with the shared placeholder + cursor
/// styling. ratatui-textarea has no public "clear", so resetting the
/// composer means swapping in a fresh one from here.
fn new_composer_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text(" Message the agent…  @ for files, / for commands");
    ta.set_cursor_line_style(ratatui::style::Style::default());
    ta
}

/// Lifecycle of the workspace file list backing the `@`-mention picker.
/// Distinguishes "not fetched yet", "in flight", "loaded" (possibly
/// empty), and "failed" so the picker can render the right placeholder.
#[derive(Debug, Clone)]
pub enum FileIndex {
    Unloaded,
    Loading,
    Loaded { files: Vec<String>, truncated: bool },
    Failed(String),
}

/// Open-picker UI state. Selection only; see [`StructuredViewState::mention`].
#[derive(Debug, Clone, Default)]
pub struct MentionSession {
    pub selected: usize,
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

impl StructuredViewState {
    pub fn new(
        session_id: String,
        endpoint: DaemonEndpoint,
        http: HttpClient,
        ws: Option<WsHandle>,
    ) -> Self {
        Self {
            transcript: AcpTranscript::new(session_id.clone()),
            session_id,
            endpoint,
            http,
            composer: new_composer_textarea(),
            focus: Focus::Composer,
            scroll_offset: u16::MAX, // stick to bottom by default; render clamps to last row
            selected_approval: None,
            ws,
            toast: None,
            queue: PromptQueue::default(),
            in_flight: false,
            slash_selected: 0,
            dismissed_slash_query: None,
            file_index: FileIndex::Unloaded,
            path_roots: None,
            mention: None,
            dismissed_mention: None,
            recall: None,
            plugin_ui: UiSnapshot {
                entries: Vec::new(),
                notifications: Vec::new(),
                revisions: Default::default(),
            },
            plugin_notify: PluginNotifyState::default(),
            layout: None,
            choice: None,
            auto_presented_elicitation: None,
            last_scroll_max: std::cell::Cell::new(0),
        }
    }

    /// Fold a freshly-polled plugin UI snapshot into state: store it for the
    /// renderer and enqueue any genuinely-new notifications for this session
    /// as toasts. The first snapshot only baselines the seq watermark (so
    /// notifications that predate opening the view stay silent); a snapshot
    /// whose max seq regressed below the watermark is treated as a daemon
    /// restart (seq ring reset) and re-baselines without toasting.
    pub fn ingest_plugin_ui(&mut self, snapshot: UiSnapshot) {
        let max_seq = plugin_ui::max_notification_seq(&snapshot);
        if !self.plugin_notify.initialized || max_seq < self.plugin_notify.last_seen_seq {
            self.plugin_notify.last_seen_seq = max_seq;
            self.plugin_notify.initialized = true;
        } else {
            for n in plugin_ui::new_notifications(
                &snapshot,
                self.plugin_notify.last_seen_seq,
                &self.session_id,
            ) {
                self.plugin_notify.pending.push_back(n.clone());
                while self.plugin_notify.pending.len() > MAX_PENDING_PLUGIN_TOASTS {
                    self.plugin_notify.pending.pop_front();
                }
            }
            self.plugin_notify.last_seen_seq = max_seq;
        }
        self.plugin_ui = snapshot;
    }

    /// Pop the next buffered plugin notification to show, if any.
    pub fn next_plugin_toast(&mut self) -> Option<Notification> {
        self.plugin_notify.pending.pop_front()
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
        self.composer = new_composer_textarea();
        self.slash_selected = 0;
        self.dismissed_slash_query = None;
        // The fresh composer holds no `@`-token; close the mention picker
        // too. The fetched file_index cache survives for the session.
        self.mention = None;
        self.dismissed_mention = None;
        // A submit / reset ends any queue-recall browse.
        self.recall = None;
        text
    }

    /// True when the composer caret sits at the very start (row 0, col 0).
    /// An empty composer trivially satisfies this. Gates entry into
    /// queue-recall so ArrowUp keeps moving the caret inside a multi-line
    /// draft until the user is at the top-left, then falls through to the
    /// queue like a shell history.
    pub fn caret_at_origin(&self) -> bool {
        self.composer.cursor() == (0, 0)
    }

    /// Whether an ArrowUp/ArrowDown queue browse is active.
    pub fn browsing_queue(&self) -> bool {
        self.recall.is_some()
    }

    /// Replace the composer contents with `text`, caret at the end.
    /// Mirrors [`take_composer_text`]'s fresh-textarea swap since
    /// ratatui-textarea has no public clear.
    fn set_composer_text(&mut self, text: &str) {
        self.composer = new_composer_textarea();
        self.composer.insert_str(text);
        self.slash_selected = 0;
        self.dismissed_slash_query = None;
        self.mention = None;
        self.dismissed_mention = None;
    }

    /// Step the queue-recall browse by `delta` (-1 = older via ArrowUp,
    /// +1 = newer via ArrowDown). Entering from the normal composer
    /// stashes the current draft; walking past the newest entry restores
    /// it and exits browse. A no-op on an empty queue.
    pub fn recall_step(&mut self, delta: i32) {
        let len = self.queue.len();
        if len == 0 {
            self.recall = None;
            return;
        }
        match self.recall.take() {
            None => {
                // Only ArrowUp enters browse; ArrowDown with no browse is a
                // no-op (the dispatcher already gates this, but stay safe).
                if delta >= 0 {
                    return;
                }
                let stashed_draft = self.composer.lines().join("\n");
                let index = len - 1;
                self.set_composer_text(&self.queue.get(index).cloned().unwrap_or_default());
                self.recall = Some(RecallState {
                    index,
                    stashed_draft,
                });
            }
            Some(mut r) => {
                if delta < 0 {
                    // Older: stop at the oldest entry, no wrap.
                    if r.index > 0 {
                        r.index -= 1;
                        self.set_composer_text(
                            &self.queue.get(r.index).cloned().unwrap_or_default(),
                        );
                    }
                    self.recall = Some(r);
                } else {
                    // Newer: past the newest entry, restore the stashed draft.
                    if r.index + 1 < len {
                        r.index += 1;
                        self.set_composer_text(
                            &self.queue.get(r.index).cloned().unwrap_or_default(),
                        );
                        self.recall = Some(r);
                    } else {
                        let draft = r.stashed_draft.clone();
                        self.set_composer_text(&draft);
                        self.recall = None;
                    }
                }
            }
        }
    }

    /// Reconcile an active browse after `dropped` entries drain off the
    /// front of the queue. The browsed entry shifts down by `dropped`; if
    /// it was among the drained ones (or the queue emptied) the browse is
    /// cancelled, leaving the in-progress composer text as a normal draft.
    pub fn reconcile_recall_after_drain(&mut self, dropped: usize) {
        if let Some(r) = self.recall.as_mut() {
            if r.index < dropped || self.queue.is_empty() {
                self.recall = None;
            } else {
                r.index -= dropped;
            }
        }
    }

    /// End a queue-recall browse without touching the composer text, so
    /// any edited prompt is retained as a draft. Called when the queue is
    /// cleared or focus leaves the composer mid-browse.
    pub fn cancel_recall(&mut self) {
        self.recall = None;
    }

    /// Abandon a queue-recall browse and restore the draft that was in the
    /// composer before browsing began (the `Esc` while browsing).
    pub fn recall_cancel_restore(&mut self) {
        if let Some(r) = self.recall.take() {
            let draft = r.stashed_draft;
            self.set_composer_text(&draft);
        }
    }

    /// The current single-line slash query (without the leading slash),
    /// or `None` when the composer doesn't hold one.
    pub fn slash_query(&self) -> Option<String> {
        let line = self.composer.lines().join("\n");
        slash::slash_query(&line).map(str::to_string)
    }

    /// Commands matching the current slash query, ranked. Empty when
    /// the composer isn't a slash query.
    pub fn slash_matches(&self) -> Vec<&AvailableCommand> {
        match self.slash_query() {
            Some(q) => slash::filter_commands(&q, &self.transcript.available_commands),
            None => Vec::new(),
        }
    }

    /// The picker is open when the composer holds a slash query that has
    /// matches and the user hasn't dismissed *this exact* query.
    pub fn slash_picker_open(&self) -> bool {
        let Some(query) = self.slash_query() else {
            return false;
        };
        if self.dismissed_slash_query.as_deref() == Some(query.as_str()) {
            return false;
        }
        !self.slash_matches().is_empty()
    }

    /// Move the picker highlight by `delta` rows, saturating at both
    /// ends of the live match list.
    pub fn move_slash_selection(&mut self, delta: i32) {
        let len = self.slash_matches().len();
        if len == 0 {
            self.slash_selected = 0;
            return;
        }
        let max = len - 1;
        let next = self.slash_selected as i64 + delta as i64;
        self.slash_selected = next.clamp(0, max as i64) as usize;
    }

    /// Latch the current query as dismissed so the picker closes until
    /// the query text changes.
    pub fn dismiss_slash(&mut self) {
        self.dismissed_slash_query = self.slash_query();
    }

    /// Replace the composer with `/{name} ` (trailing space, ready for
    /// arguments) for the highlighted command. Does not submit. Returns
    /// false when there's no match to accept.
    pub fn accept_selected_slash(&mut self) -> bool {
        let name = match self.slash_matches().get(self.slash_selected) {
            Some(cmd) => cmd.name.clone(),
            None => return false,
        };
        let mut next = new_composer_textarea();
        next.insert_str(format!("/{name} "));
        self.composer = next;
        self.slash_selected = 0;
        self.dismissed_slash_query = None;
        true
    }

    /// Keep `slash_selected` in bounds and reset the dismissal latch
    /// when the query text changes. Call after every composer edit and
    /// whenever the available-command list shifts under the cursor.
    pub fn reconcile_slash_selection(&mut self) {
        // A query change clears the dismissal so a freshly-typed query
        // reopens the picker even if its text once matched a dismissed
        // one earlier in the session.
        let query = self.slash_query();
        if self.dismissed_slash_query.is_some() && self.dismissed_slash_query != query {
            self.dismissed_slash_query = None;
        }
        let len = self.slash_matches().len();
        if len == 0 {
            self.slash_selected = 0;
        } else if self.slash_selected >= len {
            self.slash_selected = len - 1;
        }
    }

    /// Bring the selected-approval index back into bounds whenever the
    /// pending list changes underneath us (a resolution removed one,
    /// a new request added one, etc.).
    pub fn reconcile_selection(&mut self) {
        let len = self.transcript.pending_approvals.len();
        if len == 0 {
            self.selected_approval = None;
            // The prompt is gone; hand the keyboard back to the composer.
            if matches!(self.focus, Focus::Approval) {
                self.focus = Focus::Composer;
            }
            return;
        }
        match self.selected_approval {
            Some(i) if i >= len => self.selected_approval = Some(len - 1),
            None => self.selected_approval = Some(0),
            _ => {}
        }
        // A pending approval is modal, like a native agent's permission
        // prompt: it grabs focus so the decision keys work with no Tab
        // into the chat. A menu the user opened themselves (mode) keeps
        // the keyboard until dismissed.
        if matches!(self.focus, Focus::Composer) && self.choice.is_none() {
            self.focus = Focus::Approval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::Source;

    fn test_state(ws: Option<WsHandle>) -> StructuredViewState {
        let endpoint = DaemonEndpoint {
            base_url: "http://127.0.0.1:8080".into(),
            token: None,
            source: Source::Env,
        };
        let http = HttpClient::new(endpoint.clone()).unwrap();
        StructuredViewState::new("s-1".into(), endpoint, http, ws)
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

    fn composer_text(state: &StructuredViewState) -> String {
        state.composer.lines().join("\n")
    }

    #[test]
    fn recall_browses_from_newest_and_stashes_the_draft() {
        let mut state = test_state(None);
        state.queue.push("first".into());
        state.queue.push("second".into());
        state.composer.insert_str("my draft");

        // ArrowUp loads the newest queued prompt and stashes the draft.
        state.recall_step(-1);
        assert_eq!(composer_text(&state), "second");
        assert_eq!(state.recall.as_ref().unwrap().index, 1);

        // ArrowUp again walks to the older entry.
        state.recall_step(-1);
        assert_eq!(composer_text(&state), "first");
        assert_eq!(state.recall.as_ref().unwrap().index, 0);

        // Past the oldest: no wrap, stays put.
        state.recall_step(-1);
        assert_eq!(composer_text(&state), "first");
        assert_eq!(state.recall.as_ref().unwrap().index, 0);
    }

    #[test]
    fn recall_down_past_newest_restores_the_stashed_draft() {
        let mut state = test_state(None);
        state.queue.push("only".into());
        state.composer.insert_str("draft text");

        state.recall_step(-1);
        assert_eq!(composer_text(&state), "only");
        // ArrowDown past the newest restores the draft and ends browse.
        state.recall_step(1);
        assert_eq!(composer_text(&state), "draft text");
        assert!(state.recall.is_none());
    }

    #[test]
    fn reconcile_shifts_browse_index_when_front_drains() {
        let mut state = test_state(None);
        state.queue.push("a".into());
        state.queue.push("b".into());
        state.queue.push("c".into());
        state.recall_step(-1); // browsing "c" at index 2
        assert_eq!(state.recall.as_ref().unwrap().index, 2);

        // Two entries drain off the front; the browsed entry shifts to 0.
        state.queue.drop_front(2);
        state.reconcile_recall_after_drain(2);
        assert_eq!(state.recall.as_ref().unwrap().index, 0);
    }

    #[test]
    fn reconcile_cancels_browse_when_browsed_entry_drains() {
        let mut state = test_state(None);
        state.queue.push("a".into());
        state.queue.push("b".into());
        // Browse the oldest ("a", index 0) by walking up twice.
        state.recall_step(-1);
        state.recall_step(-1);
        assert_eq!(state.recall.as_ref().unwrap().index, 0);
        let browsed = composer_text(&state);

        state.queue.drop_front(1);
        state.reconcile_recall_after_drain(1);
        // The browsed entry is gone: browse cancelled, edited text retained.
        assert!(state.recall.is_none());
        assert_eq!(composer_text(&state), browsed);
    }

    #[test]
    fn recall_cancel_restore_brings_back_the_draft() {
        let mut state = test_state(None);
        state.queue.push("queued".into());
        state.composer.insert_str("draft");
        state.recall_step(-1);
        assert_eq!(composer_text(&state), "queued");
        state.recall_cancel_restore();
        assert!(state.recall.is_none());
        assert_eq!(composer_text(&state), "draft");
    }

    #[test]
    fn cancel_recall_keeps_composer_text() {
        let mut state = test_state(None);
        state.queue.push("queued".into());
        state.recall_step(-1);
        assert_eq!(composer_text(&state), "queued");
        state.cancel_recall();
        assert!(state.recall.is_none());
        assert_eq!(composer_text(&state), "queued");
    }

    #[test]
    fn caret_at_origin_tracks_cursor() {
        let mut state = test_state(None);
        assert!(state.caret_at_origin());
        state.composer.insert_str("text");
        assert!(!state.caret_at_origin());
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

    fn cmd(name: &str) -> AvailableCommand {
        AvailableCommand {
            name: name.to_string(),
            description: String::new(),
            accepts_input: false,
        }
    }

    fn state_with_commands(names: &[&str]) -> StructuredViewState {
        let endpoint = DaemonEndpoint {
            base_url: "http://127.0.0.1:8080".to_string(),
            token: None,
            source: Source::LocalDaemon,
        };
        let http = HttpClient::new(endpoint.clone()).expect("build test http client");
        let mut state = StructuredViewState::new("test-session".to_string(), endpoint, http, None);
        state.transcript.available_commands = names.iter().map(|n| cmd(n)).collect();
        state
    }

    #[test]
    fn picker_opens_on_slash_query_with_matches() {
        let mut state = state_with_commands(&["compact", "clear"]);
        assert!(!state.slash_picker_open());
        state.composer.insert_str("/comp");
        assert!(state.slash_picker_open());
        assert_eq!(state.slash_matches()[0].name, "compact");
    }

    #[test]
    fn picker_closed_when_no_matches() {
        let mut state = state_with_commands(&["compact"]);
        state.composer.insert_str("/zzz");
        assert!(!state.slash_picker_open());
    }

    #[test]
    fn accept_inserts_command_with_trailing_space_and_does_not_submit() {
        let mut state = state_with_commands(&["compact", "clear"]);
        state.composer.insert_str("/comp");
        assert!(state.accept_selected_slash());
        assert_eq!(state.composer.lines().join("\n"), "/compact ");
        // Trailing space means the composer is no longer a bare slash
        // query, so the picker closes after accepting.
        assert!(!state.slash_picker_open());
    }

    #[test]
    fn move_selection_clamps_at_both_ends() {
        let mut state = state_with_commands(&["compact", "compactor", "comparable"]);
        state.composer.insert_str("/comp");
        assert_eq!(state.slash_selected, 0);
        state.move_slash_selection(-1);
        assert_eq!(state.slash_selected, 0, "clamps at top");
        state.move_slash_selection(99);
        assert_eq!(
            state.slash_selected,
            state.slash_matches().len() - 1,
            "clamps at bottom"
        );
    }

    #[test]
    fn dismiss_latches_query_and_reopens_on_change() {
        let mut state = state_with_commands(&["compact"]);
        state.composer.insert_str("/comp");
        assert!(state.slash_picker_open());
        state.dismiss_slash();
        assert!(
            !state.slash_picker_open(),
            "dismissed exact query stays closed"
        );
        // Typing more changes the query, which reconcile clears.
        state.composer.insert_str("a");
        state.reconcile_slash_selection();
        assert!(state.slash_picker_open(), "query change reopens picker");
    }

    #[test]
    fn reconcile_clamps_selection_when_matches_shrink() {
        let mut state = state_with_commands(&["compact", "compactor"]);
        state.composer.insert_str("/comp");
        state.move_slash_selection(1);
        assert_eq!(state.slash_selected, 1);
        // The command list shrinks under the cursor.
        state.transcript.available_commands = vec![cmd("compact")];
        state.reconcile_slash_selection();
        assert_eq!(state.slash_selected, 0);
    }

    fn notify_snapshot(notifications: serde_json::Value) -> UiSnapshot {
        serde_json::from_value(serde_json::json!({
            "entries": [],
            "notifications": notifications,
        }))
        .expect("snapshot deserializes")
    }

    #[test]
    fn first_snapshot_baselines_without_toasting() {
        // test_state uses session "s-1".
        let mut state = test_state(None);
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 5, "plugin_id": "p", "tone": "info", "title": "old"}
        ])));
        assert!(
            state.next_plugin_toast().is_none(),
            "pre-open notifications stay silent"
        );
        assert_eq!(state.plugin_notify.last_seen_seq, 5);
    }

    #[test]
    fn later_snapshot_enqueues_only_new_matching_once() {
        let mut state = test_state(None);
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 1, "plugin_id": "p", "tone": "info", "title": "old"}
        ])));
        // seq 2 (global) and seq 3 (this session) are new; seq 4 targets
        // another session and must not toast here.
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 1, "plugin_id": "p", "tone": "info", "title": "old"},
            {"seq": 2, "plugin_id": "p", "tone": "info", "title": "global"},
            {"seq": 3, "plugin_id": "p", "tone": "warn", "title": "mine", "session_id": "s-1"},
            {"seq": 4, "plugin_id": "p", "tone": "info", "title": "other", "session_id": "s-2"}
        ])));
        assert_eq!(state.next_plugin_toast().unwrap().title, "global");
        assert_eq!(state.next_plugin_toast().unwrap().title, "mine");
        assert!(state.next_plugin_toast().is_none());

        // Re-polling the same snapshot enqueues nothing new.
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 4, "plugin_id": "p", "tone": "info", "title": "other", "session_id": "s-2"}
        ])));
        assert!(state.next_plugin_toast().is_none());
    }

    #[test]
    fn pending_queue_is_bounded_dropping_oldest() {
        let mut state = test_state(None);
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([])));
        let many: Vec<serde_json::Value> = (1..=MAX_PENDING_PLUGIN_TOASTS as u64 + 5)
            .map(|seq| serde_json::json!({"seq": seq, "plugin_id": "p", "tone": "info", "title": format!("n{seq}")}))
            .collect();
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!(many)));
        assert_eq!(state.plugin_notify.pending.len(), MAX_PENDING_PLUGIN_TOASTS);
        // Oldest dropped: the front is n6, not n1.
        assert_eq!(state.next_plugin_toast().unwrap().title, "n6");
    }

    #[test]
    fn seq_rollback_rebaselines_without_toasting() {
        let mut state = test_state(None);
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 50, "plugin_id": "p", "tone": "info", "title": "high"}
        ])));
        // Daemon restarts: seq ring resets, max seq drops below the watermark.
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 1, "plugin_id": "p", "tone": "info", "title": "fresh"}
        ])));
        assert!(state.next_plugin_toast().is_none());
        assert_eq!(state.plugin_notify.last_seen_seq, 1);
    }
}
