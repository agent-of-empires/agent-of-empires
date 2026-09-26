//! Owned state for an open structured view: focus, transcript, composer text,
//! and the websocket handle. Side effects run in the async loop in [`super`],
//! so this state stays freely borrowable by the render layer.

use std::collections::VecDeque;

use ratatui::layout::Rect;
use ratatui_textarea::TextArea;

use super::input::Focus;
use super::queue::QueueMirror;
use super::reducer::AcpTranscript;
use super::slash;
use crate::acp::client::{DaemonEndpoint, HttpClient, PluginCommandView, WsHandle};
use crate::acp::session_paths::SessionPathRoots;
use crate::acp::state::AvailableCommand;
use crate::daemon::QueuedPromptEntry;
use crate::plugin::ui_state::{Notification, UiSnapshot};
use crate::tui::plugin_ui;

/// Most plugin notifications buffered awaiting a free toast slot. The daemon
/// already bounds its ring; this second guard drops old un-shown toasts rather
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
    /// Nonce of the approval currently holding the modal action shelf. Identity
    /// is used instead of a list index so historical resolved rows can never
    /// make the highlighted request diverge from the request the keys resolve.
    pub selected_approval: Option<String>,
    pub ws: Option<WsHandle>,
    /// Toast banner that appears briefly above the composer, e.g.
    /// "prompt sent" or an HTTP error.
    pub toast: Option<ToastBanner>,
    /// Mirror of the daemon-owned prompt queue (drained server-side at the turn
    /// edge). Refreshed from `/queue` on connect and at each turn edge, with
    /// optimistic edits in between.
    pub queue: QueueMirror,
    /// Optimistic in-flight lock: set when a prompt POST is sent, cleared when
    /// the daemon echoes the turn start / end (or the POST fails). Without it a
    /// second Enter in that window would fire a duplicate concurrent prompt.
    pub in_flight: bool,
    /// Highlighted row in the slash-command picker. Meaningful only
    /// while the picker is open; clamped against the live match count.
    pub slash_selected: usize,
    /// The exact slash query dismissed with Esc. The picker stays closed while
    /// the composer text still maps to it, so cursor motion (reported as edits)
    /// cannot reopen it; it reappears once the query text changes.
    pub dismissed_slash_query: Option<String>,
    /// Workspace file list for the `@`-mention picker, fetched once per
    /// session on the first `@` and cached for its lifetime.
    pub file_index: FileIndex,
    /// Session roots used to shorten tool-call paths in the transcript.
    /// Fetched best-effort from the daemon; `None` keeps raw paths.
    pub path_roots: Option<SessionPathRoots>,
    /// `Some` while the `@`-mention picker is open, holding only the highlighted
    /// row; the query and token range are recomputed from the composer via
    /// [`super::mention::active_mention`], the single source of truth.
    pub mention: Option<MentionSession>,
    /// Anchor `(row, col)` of an `@`-token dismissed with Esc. Keeps the picker
    /// shut while the user types in that token; cleared once the token goes away.
    pub dismissed_mention: Option<(usize, usize)>,
    /// Active ArrowUp/ArrowDown queue-recall browse, or `None` when the
    /// composer is in its normal typing mode. See [`RecallState`].
    pub recall: Option<RecallState>,
    /// Latest plugin UI-state snapshot polled from the daemon (#2402). The status
    /// line renders the TUI-applicable slots (global `StatusBar`, this session's
    /// `DetailBadge`) from it.
    pub plugin_ui: UiSnapshot,
    /// The daemon's active plugin commands, polled alongside `plugin_ui`. Plugin
    /// chords resolve against this, not the TUI's local registry, so a session on
    /// a remote daemon can drive plugins installed only there (#2528).
    pub plugin_commands: Vec<PluginCommandView>,
    /// Notification bookkeeping so each `ui.notify` toasts once. See
    /// [`PluginNotifyState`].
    pub plugin_notify: PluginNotifyState,
    /// Vertical scroll of the plugin pane panel (#2467), in wrapped rows.
    /// `u16::MAX` sticks to the bottom; reset to 0 each time the panel opens.
    pub pane_scroll: u16,
    /// The pane panel's maximum scroll offset from the last render, so a scroll
    /// step can resolve the `u16::MAX` sentinel to a concrete row. Interior-mutable
    /// because the render borrows the state immutably. See `apply_pane_scroll`.
    pub last_pane_scroll_max: std::cell::Cell<u16>,
    /// Pane rectangles of the most recent draw, so mouse events hit-test against
    /// what is on screen. `None` until the first frame renders.
    pub layout: Option<ViewLayout>,
    /// Floating single-choice picker: the permission-mode picker (Shift+Tab), or
    /// the auto-opened answer menu for a pending single-select question. While
    /// open it owns Up/Down/Enter/Esc, whatever the focus.
    pub choice: Option<ChoicePicker>,
    /// Nonce of the elicitation whose answer menu was last auto-opened, so a
    /// pending question presents itself once rather than on every frame.
    pub auto_presented_elicitation: Option<String>,
    /// The maximum scroll offset from the last render, so a wheel/PageUp step can
    /// resolve the `u16::MAX` "stick to bottom" sentinel to a concrete position.
    /// Interior-mutable: the render borrows the state immutably. See `apply_scroll`.
    pub last_scroll_max: std::cell::Cell<u16>,
    /// Context-window percentage at which the status line nudges toward `/compact`,
    /// or `None` when off or the fetch failed. Read from the daemon's `/api/about`,
    /// not local config, so a remote-attached view honours that daemon (#3253).
    pub compaction_reminder_percent: Option<u8>,
}

/// One open choice-picker: a titled option list plus what accepting means.
/// Generic so the mode picker and the elicitation answer flow share routing.
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
    /// A numbered plugin-link picker: the option `value` is the URL to open.
    /// Shown when an `open-ui-link` chord resolves to more than one link.
    OpenLink,
    /// Accepting POSTs the approval with the chosen `option_id`, for a request
    /// whose options carry a question rather than an allow/deny vocabulary (#3741).
    Approval { nonce: String },
    /// Accepting records the answer and either advances to the next single-select
    /// question or, when `remaining` is empty, POSTs the accumulated answers.
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

/// Where each pane landed in the last-rendered frame, in screen coordinates.
/// The input layer maps mouse clicks to focus regions against it.
#[derive(Debug, Clone, Copy)]
pub struct ViewLayout {
    pub transcript: Rect,
    pub status: Rect,
    pub approval: Rect,
    pub queue: Rect,
    pub composer: Rect,
}

/// Tracks which plugin notifications have been shown and buffers any awaiting a
/// free toast slot, so a poll returning several shows them in turn.
#[derive(Debug, Default)]
pub struct PluginNotifyState {
    /// Highest notification seq already accounted for. Notifications at or
    /// below this are not re-toasted.
    pub last_seen_seq: u64,
    /// False until the first snapshot establishes the baseline: notifications
    /// that predate opening the view must not toast.
    pub initialized: bool,
    /// Notifications waiting to be shown, oldest first.
    pub pending: VecDeque<Notification>,
}

/// In-progress shell-history-style browse of the prompt queue. `index` points at
/// the queued entry loaded into the composer; `stashed_draft` holds the text from
/// before browsing, restored when ArrowDown walks past the newest entry.
#[derive(Debug, Clone)]
pub struct RecallState {
    pub index: usize,
    pub stashed_draft: String,
}

/// Build a composer textarea with the shared placeholder + cursor styling.
/// ratatui-textarea has no public "clear", so resetting means a fresh one.
fn new_composer_textarea() -> TextArea<'static> {
    let mut ta = TextArea::default();
    ta.set_placeholder_text("Message the agent…  @ files  / commands");
    ta.set_cursor_line_style(ratatui::style::Style::default());
    ta
}

/// Lifecycle of the workspace file list backing the `@`-mention picker, so the
/// picker can render the right placeholder for each state.
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
            queue: QueueMirror::default(),
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
            plugin_commands: Vec::new(),
            plugin_notify: PluginNotifyState::default(),
            pane_scroll: 0,
            last_pane_scroll_max: std::cell::Cell::new(0),
            layout: None,
            choice: None,
            auto_presented_elicitation: None,
            last_scroll_max: std::cell::Cell::new(0),
            compaction_reminder_percent: None,
        }
    }

    /// Drop the plugin pane overlay if up, returning focus to the transcript
    /// (#2467). It is modal and keyed off focus alone, so anything taking the
    /// keyboard away has to close it or it paints on, undismissable.
    pub fn close_plugin_pane(&mut self) {
        if matches!(self.focus, Focus::Pane) {
            self.focus = Focus::Transcript;
        }
    }

    /// Fold a freshly-polled plugin UI snapshot into state and enqueue
    /// genuinely-new notifications for this session. The first snapshot only
    /// baselines the seq watermark; one whose max seq regressed is treated as a
    /// daemon restart and re-baselines without toasting.
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

    /// Whether the composer treats Enter as send-vs-park and whether the
    /// empty-Enter queue resync fires: mid-turn, a POST in flight, or the
    /// WebSocket down. Deliberately excludes the display-only
    /// `AcpTranscript::background_agent_active`, whose turn is genuinely idle
    /// (#4001).
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

    /// True when the composer caret sits at row 0, col 0 (an empty composer
    /// trivially qualifies). Gates entry into queue-recall, so ArrowUp keeps
    /// moving inside a multi-line draft until the top-left, then falls through
    /// to the queue like shell history.
    pub fn caret_at_origin(&self) -> bool {
        self.composer.cursor() == (0, 0)
    }

    pub fn browsing_queue(&self) -> bool {
        self.recall.is_some()
    }

    /// Replace the composer contents with `text`, caret at the end.
    pub(crate) fn set_composer_text(&mut self, text: &str) {
        self.composer = new_composer_textarea();
        self.composer.insert_str(text);
        self.slash_selected = 0;
        self.dismissed_slash_query = None;
        self.mention = None;
        self.dismissed_mention = None;
    }

    /// Step the queue-recall browse by `delta` (-1 older, +1 newer). Entering
    /// from the normal composer stashes the draft; walking past the newest
    /// restores it and exits browse. A no-op on an empty queue.
    pub fn recall_step(&mut self, delta: i32) {
        let len = self.queue.len();
        if len == 0 {
            self.recall = None;
            return;
        }
        match self.recall.take() {
            None => {
                // Only ArrowUp enters browse; ArrowDown without one is a no-op.
                if delta >= 0 {
                    return;
                }
                let stashed_draft = self.composer.lines().join("\n");
                let index = len - 1;
                // Own the recalled text so the `&self.queue` borrow ends before
                // the `&mut self` set_composer_text call.
                let recalled = self.queue.text_at(index).unwrap_or_default().to_owned();
                self.set_composer_text(&recalled);
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
                        let recalled = self.queue.text_at(r.index).unwrap_or_default().to_owned();
                        self.set_composer_text(&recalled);
                    }
                    self.recall = Some(r);
                } else {
                    // Newer: past the newest entry, restore the stashed draft.
                    if r.index + 1 < len {
                        r.index += 1;
                        let recalled = self.queue.text_at(r.index).unwrap_or_default().to_owned();
                        self.set_composer_text(&recalled);
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

    /// Replace the queue mirror with a fresh daemon snapshot, keeping an active
    /// browse pointed at the same entry across a server-side drain. The entry is
    /// tracked by stable id, so drains reindex it correctly; if it is gone the
    /// browse ends, leaving the composer text as a normal draft.
    pub fn set_queue_snapshot(&mut self, entries: Vec<QueuedPromptEntry>) {
        let browsed_id = self
            .recall
            .as_ref()
            .and_then(|r| self.queue.id_at(r.index).map(str::to_string));
        self.queue.set_snapshot(entries);
        if let Some(r) = self.recall.as_mut() {
            match browsed_id.and_then(|id| self.queue.index_of(&id)) {
                Some(idx) => r.index = idx,
                None => self.recall = None,
            }
        }
    }

    /// End a queue-recall browse without touching the composer text, so an
    /// edited prompt is retained as a draft.
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

    /// Move the picker highlight by `delta` rows, saturating at both ends.
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

    /// Replace the composer with `/{name} ` for the highlighted command. Does
    /// not submit; false when there is no match to accept.
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

    /// Keep `slash_selected` in bounds and reset the dismissal latch when the
    /// query text changes. Call after every composer edit and list shift.
    pub fn reconcile_slash_selection(&mut self) {
        // A query change clears the dismissal so a freshly-typed query reopens
        // the picker even if its text once matched a dismissed one.
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

    /// Keep the selected approval tied to a pending nonce whenever the list
    /// changes underneath us.
    pub fn reconcile_selection(&mut self) {
        if self.transcript.pending_approvals.is_empty() {
            self.selected_approval = None;
            // The prompt is gone; hand the keyboard back to the composer.
            if matches!(self.focus, Focus::Approval) {
                self.focus = Focus::Composer;
            }
            return;
        }
        let selected_still_pending = self.selected_approval.as_ref().is_some_and(|selected| {
            self.transcript
                .pending_approvals
                .iter()
                .any(|pending| pending.nonce == *selected)
        });
        if !selected_still_pending {
            self.selected_approval = self
                .transcript
                .pending_approvals
                .first()
                .map(|pending| pending.nonce.clone());
        }
        // A pending approval is modal, like a native agent's permission prompt:
        // it grabs focus so decision keys work with no Tab into the chat. A menu
        // the user opened themselves keeps the keyboard until dismissed.
        if self.choice.is_none() {
            self.focus = Focus::Approval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::client::Source;

    fn test_state(ws: Option<WsHandle>) -> StructuredViewState {
        let endpoint = DaemonEndpoint::new("http://127.0.0.1:8080".into(), None, Source::Env);
        let http = HttpClient::new(endpoint.clone()).unwrap();
        StructuredViewState::new("s-1".into(), endpoint, http, ws)
    }

    #[test]
    fn fresh_state_is_idle_but_busy_while_socket_down() {
        // A dropped WebSocket (ws = None) must force queuing, since turn
        // boundaries can't be observed to drive an immediate send.
        let state = test_state(None);
        assert!(!state.transcript.turn_active);
        assert!(!state.in_flight);
        assert!(state.is_busy());
    }

    fn composer_text(state: &StructuredViewState) -> String {
        state.composer.lines().join("\n")
    }

    #[test]
    fn recall_browses_from_newest_and_restores_the_stashed_draft() {
        let mut state = test_state(None);
        state.queue.push("first".into());
        state.queue.push("second".into());
        state.composer.insert_str("my draft");

        // ArrowUp loads the newest queued prompt and stashes the draft, then
        // walks older without wrapping past the oldest.
        for (text, index) in [("second", 1), ("first", 0), ("first", 0)] {
            state.recall_step(-1);
            assert_eq!(composer_text(&state), text);
            assert_eq!(state.recall.as_ref().unwrap().index, index);
        }
        // ArrowDown past the newest restores the draft and ends browse.
        state.recall_step(1);
        state.recall_step(1);
        assert_eq!(composer_text(&state), "my draft");
        assert!(state.recall.is_none());

        // Cancel-restore brings back the draft; plain cancel keeps the text.
        state.recall_step(-1);
        state.recall_cancel_restore();
        assert!(state.recall.is_none());
        assert_eq!(composer_text(&state), "my draft");
        state.recall_step(-1);
        state.cancel_recall();
        assert!(state.recall.is_none());
        assert_eq!(composer_text(&state), "second");
    }

    #[test]
    fn snapshot_refresh_shifts_browse_index_when_front_drains() {
        let mut state = test_state(None);
        state.queue.push("a".into());
        state.queue.push("b".into());
        state.queue.push("c".into());
        state.recall_step(-1); // browsing "c" at index 2
        assert_eq!(state.recall.as_ref().unwrap().index, 2);

        // Two entries drain server-side; refresh with just "c" remaining. The
        // browse follows "c" by id to its new front position.
        let c = state.queue.entries()[2].clone();
        state.set_queue_snapshot(vec![c]);
        assert_eq!(state.recall.as_ref().unwrap().index, 0);
        assert_eq!(composer_text(&state), "c");
    }

    #[test]
    fn snapshot_refresh_cancels_browse_when_browsed_entry_drains() {
        let mut state = test_state(None);
        state.queue.push("a".into());
        state.queue.push("b".into());
        // Browse the oldest ("a", index 0) by walking up twice.
        state.recall_step(-1);
        state.recall_step(-1);
        assert_eq!(state.recall.as_ref().unwrap().index, 0);
        let browsed = composer_text(&state);

        // "a" drained; refresh with only "b". The browsed entry is gone, so the
        // browse ends with the edited text retained as a draft.
        let b = state.queue.entries()[1].clone();
        state.set_queue_snapshot(vec![b]);
        assert!(state.recall.is_none());
        assert_eq!(composer_text(&state), browsed);
    }

    fn cmd(name: &str) -> AvailableCommand {
        AvailableCommand {
            name: name.to_string(),
            description: String::new(),
            accepts_input: false,
        }
    }

    fn state_with_commands(names: &[&str]) -> StructuredViewState {
        let endpoint = DaemonEndpoint::new(
            "http://127.0.0.1:8080".to_string(),
            None,
            Source::LocalDaemon,
        );
        let http = HttpClient::new(endpoint.clone()).expect("build test http client");
        let mut state = StructuredViewState::new("test-session".to_string(), endpoint, http, None);
        state.transcript.available_commands = names.iter().map(|n| cmd(n)).collect();
        state
    }

    #[test]
    fn slash_picker_opens_on_matches_and_accept_inserts_without_submitting() {
        let mut miss = state_with_commands(&["compact"]);
        miss.composer.insert_str("/zzz");
        assert!(!miss.slash_picker_open(), "no matches keeps it closed");
        let mut state = state_with_commands(&["compact", "clear"]);
        assert!(!state.slash_picker_open());
        state.composer.insert_str("/comp");
        assert!(state.slash_picker_open());
        assert_eq!(state.slash_matches()[0].name, "compact");
        assert!(state.accept_selected_slash());
        assert_eq!(composer_text(&state), "/compact ");
        // Trailing space means the composer is no longer a bare slash
        // query, so the picker closes after accepting.
        assert!(!state.slash_picker_open());
    }

    #[test]
    fn slash_selection_clamps_and_follows_shrinking_matches() {
        let mut state = state_with_commands(&["compact", "compactor", "comparable"]);
        state.composer.insert_str("/comp");
        assert_eq!(state.slash_selected, 0);
        state.move_slash_selection(-1);
        assert_eq!(state.slash_selected, 0, "clamps at top");
        state.move_slash_selection(99);
        assert_eq!(state.slash_selected, 2, "clamps at bottom");
        // The command list shrinks under the cursor.
        state.transcript.available_commands = vec![cmd("compact")];
        state.reconcile_slash_selection();
        assert_eq!(state.slash_selected, 0);
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

    fn notify_snapshot(notifications: serde_json::Value) -> UiSnapshot {
        serde_json::from_value(serde_json::json!({
            "entries": [],
            "notifications": notifications,
        }))
        .expect("snapshot deserializes")
    }

    #[test]
    fn first_snapshot_and_seq_rollback_rebaseline_without_toasting() {
        // test_state uses session "s-1".
        let mut state = test_state(None);
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 50, "plugin_id": "p", "tone": "info", "title": "old"}
        ])));
        assert!(
            state.next_plugin_toast().is_none(),
            "pre-open notifications stay silent"
        );
        assert_eq!(state.plugin_notify.last_seen_seq, 50);
        // Daemon restarts: seq ring resets, max seq drops below the watermark.
        state.ingest_plugin_ui(notify_snapshot(serde_json::json!([
            {"seq": 1, "plugin_id": "p", "tone": "info", "title": "fresh"}
        ])));
        assert!(state.next_plugin_toast().is_none());
        assert_eq!(state.plugin_notify.last_seen_seq, 1);
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
    fn approval_selection_tracks_nonce_as_pending_list_advances() {
        use super::super::reducer::PendingApproval;

        let pending = |nonce: &str| PendingApproval {
            nonce: nonce.into(),
            title: "Read file".into(),
            kind: "read".into(),
            args: String::new(),
            destructive: false,
            options: Vec::new(),
            choice: false,
        };
        let mut state = test_state(None);
        state.transcript.pending_approvals = vec![pending("approval-b"), pending("approval-c")];
        state.reconcile_selection();
        assert_eq!(state.selected_approval.as_deref(), Some("approval-b"));
        assert_eq!(state.focus, Focus::Approval);

        state.transcript.pending_approvals.remove(0);
        state.reconcile_selection();
        assert_eq!(state.selected_approval.as_deref(), Some("approval-c"));

        state.transcript.pending_approvals.clear();
        state.reconcile_selection();
        assert!(state.selected_approval.is_none());
        assert_eq!(state.focus, Focus::Composer);
    }
}
