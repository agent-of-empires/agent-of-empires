//! Focus model + key dispatch for the structured view.
//!
//! The view is meant to feel like a native coding agent. The composer
//! is the home base: the view lands there so you can type immediately,
//! and reading history never requires a focus switch (the mouse wheel
//! and `PageUp`/`PageDown` scroll the transcript from the composer).
//! `Ctrl-Q` leaves the view, mirroring live-send's exit chord; `Esc` is
//! an agent-style interrupt (it cancels a generating turn, and is a
//! no-op when idle), never an exit. The transcript is a secondary focus
//! reached with `Tab` for its power keys (scroll, mode picker, browser,
//! elicitation answers); `Esc` there returns to the composer. The
//! composer captures **every** typed key, including `a`/`A`/`d`, so
//! typing "always allow" into a prompt never resolves an approval; a
//! pending approval is resolved by `Tab`-ing to its card (or clicking
//! it) and then pressing `a`/`A`/`d`.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::state::ViewLayout;
use crate::acp::protocol::ApprovalDecisionWire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Transcript,
    Approval,
}

/// What the input dispatcher decided to do with this key. The view
/// layer handles the actual side-effects so input.rs stays a pure
/// translator.
#[derive(Debug, Clone, PartialEq)]
pub enum Intent {
    /// Pass the key through to the composer textarea.
    Compose(KeyEvent),
    /// Submit the composer's buffered text as a prompt.
    SubmitPrompt,
    /// Scroll the transcript by N lines (positive = down).
    Scroll(i32),
    /// Resolve the focused approval card.
    ResolveApproval(ApprovalDecisionWire),
    /// Skip the oldest pending elicitation (ACP `decline`): the agent
    /// continues with no answer. The rich answer form is web-only.
    SkipElicitation,
    /// Cancel the oldest pending elicitation (ACP `cancel`): aborts the
    /// agent's tool call.
    CancelElicitation,
    /// Cancel the in-flight prompt (Ctrl-C style).
    CancelInFlight,
    /// Drop every queued (not-yet-sent) prompt.
    ClearQueue,
    /// Browse the prompt queue from the composer (shell-history style):
    /// negative steps toward older entries (ArrowUp), positive toward
    /// newer (ArrowDown). The view layer loads the entry into the composer
    /// for editing.
    RecallQueued(i32),
    /// Abandon an in-progress queue browse, restoring the stashed draft to
    /// the composer (the `Esc` while browsing).
    RecallCancel,
    /// Open the daemon URL for this session in the user's browser.
    OpenInBrowser,
    /// Move focus to the named region.
    SetFocus(Focus),
    /// Move the slash-picker highlight by one row (positive = down).
    SlashMove(i32),
    /// Insert the highlighted slash command into the composer.
    SlashAccept,
    /// Dismiss the slash picker without inserting, latching the query.
    SlashDismiss,
    /// Move the `@`-mention picker highlight by N rows (positive = down).
    MentionNavigate(i32),
    /// Insert the highlighted mention and close the picker.
    MentionAccept,
    /// Close the mention picker without inserting.
    MentionClose,
    /// Open the permission-mode picker (transcript `m`, when the agent
    /// advertised modes).
    OpenModePicker,
    /// Open the answer picker for the oldest pending elicitation
    /// (transcript `a`). The view layer decides whether the form is
    /// natively answerable or punts to the web.
    AnswerElicitation,
    /// Move the open choice picker's highlight by N rows.
    ChoiceNavigate(i32),
    /// Pick option N (0-based) in a numbered choice picker and accept it in one
    /// keystroke (the `1`-`9` hotkeys on the plugin-link picker).
    ChoicePick(usize),
    /// Accept the choice picker's highlighted option.
    ChoiceAccept,
    /// Close the choice picker without accepting.
    ChoiceCancel,
    /// Exit the structured view; return to the home screen.
    Exit,
    /// Nothing to do (unhandled key).
    Ignore,
}

/// Ambient state the dispatcher needs beyond the raw key: whether an
/// approval is pending (gates Tab routing) and whether the slash or
/// `@`-mention picker is currently open (each claims navigation keys in
/// the composer). Passed as a struct instead of positional bools so call
/// sites stay readable.
#[derive(Debug, Clone, Copy, Default)]
pub struct InputContext {
    pub has_pending_approval: bool,
    /// A pending `AskUserQuestion` elicitation exists. Gates the
    /// transcript-focus skip/cancel keys; the answer form itself is
    /// web-only.
    pub has_pending_elicitation: bool,
    pub slash_picker_open: bool,
    pub mention_picker_open: bool,
    /// Composer caret is at row 0, col 0. Gates ArrowUp entry into
    /// queue-recall so multi-line caret movement keeps working until the
    /// user reaches the top-left.
    pub caret_at_origin: bool,
    /// A queue-recall browse is already active; while browsing, ArrowUp /
    /// ArrowDown navigate the queue regardless of caret position.
    pub browsing_queue: bool,
    /// Number of queued prompts; ArrowUp only enters recall when there is
    /// something to recall.
    pub queue_len: usize,
    /// A choice picker (mode / elicitation answer) is open; it owns
    /// Up/Down/Enter/Esc from any focus until accepted or dismissed.
    pub choice_picker_open: bool,
    /// The open choice picker is a numbered picker (the plugin-link picker), so
    /// `1`-`9` pick and accept a row directly. Off for the mode / elicitation
    /// pickers, where digits stay inert.
    pub choice_numbered: bool,
    /// The agent advertised permission modes; gates the transcript `m` key.
    pub has_modes: bool,
    /// The agent is generating (a turn is active or a prompt is in
    /// flight). Gates `Esc` in the composer: while busy it interrupts
    /// the turn like a native agent, and is an inert no-op when idle.
    pub agent_busy: bool,
}

/// Translate a key event into an [`Intent`] based on the current
/// focus. Pure function so the entire focus model is unit-testable
/// without instantiating a real ratatui surface.
pub fn dispatch(focus: Focus, key: &KeyEvent, ctx: InputContext) -> Intent {
    // Universal: Ctrl-C cancels any in-flight prompt (matches the web
    // composer's stop button). We intentionally do NOT exit the view
    // on Ctrl-C because the user's natural reflex from a tmux session
    // is "stop the agent, don't quit the screen."
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Intent::CancelInFlight;
    }
    // Universal: Ctrl-q leaves the view, mirroring live-send's exit chord
    // so "get me out" is the same reflex whether you're driving a raw
    // tmux agent or the structured view. Placed among the universal
    // chords so it works from any focus (composer, transcript, approval).
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
        return Intent::Exit;
    }
    // Universal: Ctrl-o opens the browser. `o` alone is reserved for
    // transcript-focus so typing "no" into the composer doesn't open a
    // browser tab.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
        return Intent::OpenInBrowser;
    }
    // Universal: Ctrl-x drops every queued prompt. Intercepted here,
    // before the composer sees it, so it works from any focus and a
    // queued backlog can always be abandoned without leaving the
    // composer. A no-op when the queue is empty.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
        return Intent::ClearQueue;
    }
    // An open choice picker (mode / answer) owns its navigation keys from
    // any focus: the user deliberately opened it, and it closes on
    // Enter/Esc, so nothing else needs those keys meanwhile.
    if ctx.choice_picker_open {
        match (key.modifiers, key.code) {
            // Number hotkeys on a numbered picker: pick that row and accept it
            // in one press. `1` is row 0. Only when the picker opted in.
            (m, KeyCode::Char(c))
                if m.is_empty() && ctx.choice_numbered && ('1'..='9').contains(&c) =>
            {
                return Intent::ChoicePick(c as usize - '1' as usize)
            }
            (m, KeyCode::Down) if m.is_empty() => return Intent::ChoiceNavigate(1),
            (m, KeyCode::Up) if m.is_empty() => return Intent::ChoiceNavigate(-1),
            (m, KeyCode::Char('j')) if m.is_empty() => return Intent::ChoiceNavigate(1),
            (m, KeyCode::Char('k')) if m.is_empty() => return Intent::ChoiceNavigate(-1),
            (m, KeyCode::Enter) if m.is_empty() => return Intent::ChoiceAccept,
            (m, KeyCode::Esc) if m.is_empty() => return Intent::ChoiceCancel,
            _ => return Intent::Ignore,
        }
    }

    match focus {
        Focus::Composer => composer_keys(key, ctx),
        Focus::Transcript => transcript_keys(key, ctx),
        Focus::Approval => approval_keys(key),
    }
}

/// Transcript lines scrolled per mouse-wheel tick, matching the home
/// screen's preview wheel step.
const WHEEL_SCROLL_LINES: i32 = 3;

/// Transcript lines scrolled per `PageUp`/`PageDown`, from either the
/// transcript or the composer.
const PAGE_SCROLL_LINES: i32 = 10;

/// Translate a mouse event into an [`Intent`]. The wheel always scrolls
/// the transcript (whatever pane the pointer is over; the composer and
/// status line have no scrollback of their own), and a left click moves
/// focus to the pane under the pointer. `layout` is the pane geometry of
/// the last-drawn frame; before the first draw there is nothing to
/// hit-test, so clicks are ignored.
pub fn dispatch_mouse(mouse: &MouseEvent, layout: Option<&ViewLayout>) -> Intent {
    match mouse.kind {
        MouseEventKind::ScrollUp => Intent::Scroll(-WHEEL_SCROLL_LINES),
        MouseEventKind::ScrollDown => Intent::Scroll(WHEEL_SCROLL_LINES),
        // A click anywhere in the view focuses the composer: it is one
        // pane, so clicking the chat to read never puts you in a
        // separate "transcript mode" you'd have to click back out of.
        // (`layout` is unused now but kept in the signature so a future
        // click-target, e.g. an approval button, has the geometry.)
        MouseEventKind::Down(MouseButton::Left) => {
            let _ = layout;
            Intent::SetFocus(Focus::Composer)
        }
        _ => Intent::Ignore,
    }
}

fn composer_keys(key: &KeyEvent, ctx: InputContext) -> Intent {
    let slash_picker_open = ctx.slash_picker_open;
    let mention_picker_open = ctx.mention_picker_open;
    // While browsing the queue, recall navigation owns its core keys even
    // when the recalled text would otherwise open the slash / `@` picker
    // (e.g. a queued "/clear"). Without this the picker would steal
    // Up/Down/Esc/Enter and break recall navigation, restore, and save.
    // Typed characters still fall through below to narrow the picker.
    if ctx.browsing_queue {
        match (key.modifiers, key.code) {
            (m, KeyCode::Up) if m.is_empty() => return Intent::RecallQueued(-1),
            (m, KeyCode::Down) if m.is_empty() => return Intent::RecallQueued(1),
            (m, KeyCode::Esc) if m.is_empty() => return Intent::RecallCancel,
            (m, KeyCode::Enter) if m.is_empty() => return Intent::SubmitPrompt,
            _ => {}
        }
    }
    // When a picker is open it claims navigation + accept/dismiss keys
    // so the user can drive it without the textarea swallowing them.
    // Everything else (typing, cursor motion the picker doesn't use)
    // falls through to the normal composer rules below. Slash and
    // mention pickers are mutually exclusive (a line can't both start
    // with `/` and hold an `@`-token at the cursor), but slash wins the
    // tie defensively.
    if slash_picker_open {
        match (key.modifiers, key.code) {
            (m, KeyCode::Down) if m.is_empty() => return Intent::SlashMove(1),
            (m, KeyCode::Up) if m.is_empty() => return Intent::SlashMove(-1),
            (m, KeyCode::Char('n')) if m == KeyModifiers::CONTROL => return Intent::SlashMove(1),
            (m, KeyCode::Char('p')) if m == KeyModifiers::CONTROL => return Intent::SlashMove(-1),
            (m, KeyCode::Enter) if m.is_empty() => return Intent::SlashAccept,
            (m, KeyCode::Tab) if m.is_empty() => return Intent::SlashAccept,
            (m, KeyCode::Esc) if m.is_empty() => return Intent::SlashDismiss,
            _ => {}
        }
    } else if mention_picker_open {
        match (key.modifiers, key.code) {
            (m, KeyCode::Down) if m.is_empty() => return Intent::MentionNavigate(1),
            (m, KeyCode::Up) if m.is_empty() => return Intent::MentionNavigate(-1),
            (m, KeyCode::Char('n')) if m == KeyModifiers::CONTROL => {
                return Intent::MentionNavigate(1)
            }
            (m, KeyCode::Char('p')) if m == KeyModifiers::CONTROL => {
                return Intent::MentionNavigate(-1)
            }
            (m, KeyCode::Enter) if m.is_empty() => return Intent::MentionAccept,
            (m, KeyCode::Tab) if m.is_empty() => return Intent::MentionAccept,
            (m, KeyCode::Esc) if m.is_empty() => return Intent::MentionClose,
            _ => {}
        }
    }
    match (key.modifiers, key.code) {
        // Queue recall: ArrowUp browses toward older queued prompts when
        // already browsing, or when the caret is at the top-left and the
        // queue is non-empty; ArrowDown walks back toward newer entries
        // (and the stashed draft) only while browsing. Outside those
        // conditions Up / Down fall through to normal textarea caret
        // movement so multi-line editing is unaffected.
        (m, KeyCode::Up)
            if m.is_empty()
                && (ctx.browsing_queue || (ctx.caret_at_origin && ctx.queue_len > 0)) =>
        {
            Intent::RecallQueued(-1)
        }
        (m, KeyCode::Down) if m.is_empty() && ctx.browsing_queue => Intent::RecallQueued(1),
        // Esc while browsing the queue restores the stashed draft instead
        // of leaving the composer.
        (m, KeyCode::Esc) if m.is_empty() && ctx.browsing_queue => Intent::RecallCancel,
        // Plain Enter submits.
        (m, KeyCode::Enter) if m.is_empty() => Intent::SubmitPrompt,
        // Shift+Enter inserts a newline (passed through to textarea).
        (m, KeyCode::Enter) if m.contains(KeyModifiers::SHIFT) => Intent::Compose(*key),
        // Ctrl+J is crossterm's raw-mode decoding of a bare line feed (\n),
        // which some terminals send for Shift+Enter (e.g. a Ghostty
        // `keybind = shift+enter=text:\n`). Forward a plain Enter so the
        // textarea inserts a newline; passing the raw Ctrl+J through would hit
        // the textarea's default delete-to-line-head binding and wipe the line.
        (m, KeyCode::Char('j')) if m == KeyModifiers::CONTROL => {
            Intent::Compose(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        }
        // Page keys scroll the transcript without leaving the composer,
        // so reading history never needs a focus switch (the mouse wheel
        // does the same from any focus). PageUp/PageDown aren't textarea
        // editing keys, so nothing is lost by claiming them here.
        (m, KeyCode::PageUp) if m.is_empty() => Intent::Scroll(-PAGE_SCROLL_LINES),
        (m, KeyCode::PageDown) if m.is_empty() => Intent::Scroll(PAGE_SCROLL_LINES),
        // Esc is native-agent behavior, not an exit (Ctrl-Q leaves the
        // view). While the agent is generating it interrupts the turn,
        // the same reflex as hitting Esc in a raw agent session; when
        // idle it is an inert no-op so a stray Esc never drops you out.
        // Pickers and queue-browse intercept Esc above.
        (m, KeyCode::Esc) if m.is_empty() => {
            if ctx.agent_busy {
                Intent::CancelInFlight
            } else {
                Intent::Ignore
            }
        }
        // Shift+Tab opens the permission-mode picker, mirroring Claude
        // Code's mode-cycle chord. It's the one "power" control that
        // used to live behind the transcript focus; everything else
        // (scroll, browser, exit) is reachable from the composer now, so
        // there is no Tab-to-the-chat toggle at all. crossterm reports
        // Shift+Tab as BackTab. A no-op when the agent advertised no
        // modes. Plain Tab is inert (pickers claim it above to accept).
        (_, KeyCode::BackTab) if ctx.has_modes => Intent::OpenModePicker,
        // BackTab is never text; swallow it even with no modes so it
        // can't leak into the composer as a stray character.
        (_, KeyCode::BackTab) => Intent::Ignore,
        (m, KeyCode::Tab) if m.is_empty() => Intent::Ignore,
        // Everything else is forwarded to the textarea, including
        // `a`/`A`/`d`. This is the focus-isolation guarantee.
        _ => Intent::Compose(*key),
    }
}

fn transcript_keys(key: &KeyEvent, ctx: InputContext) -> Intent {
    let has_pending_approval = ctx.has_pending_approval;
    let has_pending_elicitation = ctx.has_pending_elicitation;
    match (key.modifiers, key.code) {
        // Answer / skip / cancel a pending elicitation. Gated on a
        // pending elicitation so `a`/`s`/`c` stay free otherwise.
        (m, KeyCode::Char('a')) if m.is_empty() && has_pending_elicitation => {
            Intent::AnswerElicitation
        }
        (m, KeyCode::Char('s')) if m.is_empty() && has_pending_elicitation => {
            Intent::SkipElicitation
        }
        (m, KeyCode::Char('c')) if m.is_empty() && has_pending_elicitation => {
            Intent::CancelElicitation
        }
        // Permission-mode picker, when the agent advertised modes.
        (m, KeyCode::Char('m')) if m.is_empty() && ctx.has_modes => Intent::OpenModePicker,
        // Esc returns to the composer (the home base), not straight out:
        // the transcript is a secondary focus you Tab into for its power
        // keys, so Esc backs out one level rather than leaving the view.
        (m, KeyCode::Esc) if m.is_empty() => Intent::SetFocus(Focus::Composer),
        // Switch to composer.
        (m, KeyCode::Char('i')) if m.is_empty() => Intent::SetFocus(Focus::Composer),
        (m, KeyCode::Tab) if m.is_empty() => {
            if has_pending_approval {
                Intent::SetFocus(Focus::Approval)
            } else {
                Intent::SetFocus(Focus::Composer)
            }
        }
        // Vim-style scroll.
        (m, KeyCode::Char('j')) if m.is_empty() => Intent::Scroll(1),
        (m, KeyCode::Char('k')) if m.is_empty() => Intent::Scroll(-1),
        (m, KeyCode::Down) if m.is_empty() => Intent::Scroll(1),
        (m, KeyCode::Up) if m.is_empty() => Intent::Scroll(-1),
        (m, KeyCode::PageDown) if m.is_empty() => Intent::Scroll(PAGE_SCROLL_LINES),
        (m, KeyCode::PageUp) if m.is_empty() => Intent::Scroll(-PAGE_SCROLL_LINES),
        (m, KeyCode::Char('g')) if m.is_empty() => Intent::Scroll(i32::MIN),
        (m, KeyCode::Char('G')) if m.contains(KeyModifiers::SHIFT) => Intent::Scroll(i32::MAX),
        // Plain 'o' opens browser only when transcript is focused.
        (m, KeyCode::Char('o')) if m.is_empty() => Intent::OpenInBrowser,
        _ => Intent::Ignore,
    }
}

fn approval_keys(key: &KeyEvent) -> Intent {
    match (key.modifiers, key.code) {
        (m, KeyCode::Char('a')) if m.is_empty() => {
            Intent::ResolveApproval(ApprovalDecisionWire::Allow)
        }
        (m, KeyCode::Char('A')) if m.contains(KeyModifiers::SHIFT) => {
            Intent::ResolveApproval(ApprovalDecisionWire::AllowAlways)
        }
        (m, KeyCode::Char('d')) if m.is_empty() => {
            Intent::ResolveApproval(ApprovalDecisionWire::Deny)
        }
        // A pending approval is modal (it grabs focus like a native
        // permission prompt), so Esc interrupts the turn rather than
        // trying to "leave" the prompt: cancelling clears the request
        // and drops you back to the composer.
        (m, KeyCode::Esc) if m.is_empty() => Intent::CancelInFlight,
        _ => Intent::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn key_mod(code: KeyCode, m: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, m)
    }

    /// No pending approval, pickers closed: the common case for the
    /// pre-existing focus tests.
    fn ctx() -> InputContext {
        InputContext::default()
    }

    fn ctx_pending() -> InputContext {
        InputContext {
            has_pending_approval: true,
            ..InputContext::default()
        }
    }

    #[test]
    fn numbered_choice_picker_digit_picks_and_accepts() {
        let numbered = InputContext {
            choice_picker_open: true,
            choice_numbered: true,
            ..InputContext::default()
        };
        // `1` is row 0; `3` is row 2, from any focus.
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Char('1')), numbered),
            Intent::ChoicePick(0)
        );
        assert_eq!(
            dispatch(Focus::Transcript, &key(KeyCode::Char('3')), numbered),
            Intent::ChoicePick(2)
        );
        // Enter still accepts the highlighted row.
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Enter), numbered),
            Intent::ChoiceAccept
        );
        // A non-numbered picker (mode / elicitation) leaves digits inert.
        let plain = InputContext {
            choice_picker_open: true,
            choice_numbered: false,
            ..InputContext::default()
        };
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Char('1')), plain),
            Intent::Ignore
        );
    }

    fn ctx_picker() -> InputContext {
        InputContext {
            slash_picker_open: true,
            ..InputContext::default()
        }
    }

    fn ctx_mention() -> InputContext {
        InputContext {
            mention_picker_open: true,
            ..InputContext::default()
        }
    }

    #[test]
    fn composer_swallows_approval_letters() {
        // Regression test for the composer-eats-approval bug: typing
        // "always allow" with a pending approval must NOT fire any
        // approval intent.
        for ch in "always allow deny".chars() {
            let intent = dispatch(Focus::Composer, &key(KeyCode::Char(ch)), ctx_pending());
            match intent {
                Intent::Compose(_) => {}
                other => panic!("char {ch:?} produced {other:?} from composer focus"),
            }
        }
    }

    #[test]
    fn approval_keys_only_resolve_when_focused() {
        // Same letters from the transcript focus must NOT resolve.
        for ch in "aAd".chars() {
            let intent = dispatch(
                Focus::Transcript,
                &key_mod(
                    KeyCode::Char(ch),
                    if ch.is_uppercase() {
                        KeyModifiers::SHIFT
                    } else {
                        KeyModifiers::NONE
                    },
                ),
                ctx_pending(),
            );
            assert!(
                !matches!(intent, Intent::ResolveApproval(_)),
                "{ch} resolved from transcript focus: {intent:?}"
            );
        }
        // But the same letters DO resolve under approval focus.
        assert!(matches!(
            dispatch(Focus::Approval, &key(KeyCode::Char('a')), ctx_pending()),
            Intent::ResolveApproval(ApprovalDecisionWire::Allow)
        ));
        assert!(matches!(
            dispatch(
                Focus::Approval,
                &key_mod(KeyCode::Char('A'), KeyModifiers::SHIFT),
                ctx_pending()
            ),
            Intent::ResolveApproval(ApprovalDecisionWire::AllowAlways)
        ));
        assert!(matches!(
            dispatch(Focus::Approval, &key(KeyCode::Char('d')), ctx_pending()),
            Intent::ResolveApproval(ApprovalDecisionWire::Deny)
        ));
    }

    fn ctx_pending_elicitation() -> InputContext {
        InputContext {
            has_pending_elicitation: true,
            ..InputContext::default()
        }
    }

    fn ctx_recall(caret_at_origin: bool, browsing_queue: bool, queue_len: usize) -> InputContext {
        InputContext {
            caret_at_origin,
            browsing_queue,
            queue_len,
            ..InputContext::default()
        }
    }

    #[test]
    fn choice_picker_owns_navigation_from_any_focus() {
        let ctx = InputContext {
            choice_picker_open: true,
            ..InputContext::default()
        };
        for focus in [Focus::Composer, Focus::Transcript, Focus::Approval] {
            assert_eq!(
                dispatch(focus, &key(KeyCode::Down), ctx),
                Intent::ChoiceNavigate(1)
            );
            assert_eq!(
                dispatch(focus, &key(KeyCode::Up), ctx),
                Intent::ChoiceNavigate(-1)
            );
            assert_eq!(
                dispatch(focus, &key(KeyCode::Enter), ctx),
                Intent::ChoiceAccept
            );
            assert_eq!(
                dispatch(focus, &key(KeyCode::Esc), ctx),
                Intent::ChoiceCancel
            );
            // Other keys are swallowed while the picker is up, so a stray
            // 'a' can't resolve an approval underneath it.
            assert_eq!(
                dispatch(focus, &key(KeyCode::Char('a')), ctx),
                Intent::Ignore
            );
        }
    }

    #[test]
    fn mode_picker_key_gated_on_advertised_modes() {
        let with_modes = InputContext {
            has_modes: true,
            ..InputContext::default()
        };
        assert_eq!(
            dispatch(Focus::Transcript, &key(KeyCode::Char('m')), with_modes),
            Intent::OpenModePicker
        );
        // Without modes 'm' stays free; from the composer it types.
        assert_eq!(
            dispatch(Focus::Transcript, &key(KeyCode::Char('m')), ctx()),
            Intent::Ignore
        );
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('m')), with_modes),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn answer_key_gated_on_pending_elicitation() {
        assert_eq!(
            dispatch(
                Focus::Transcript,
                &key(KeyCode::Char('a')),
                ctx_pending_elicitation()
            ),
            Intent::AnswerElicitation
        );
        assert!(!matches!(
            dispatch(Focus::Transcript, &key(KeyCode::Char('a')), ctx()),
            Intent::AnswerElicitation
        ));
        // From the composer 'a' must type, never answer.
        assert!(matches!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Char('a')),
                ctx_pending_elicitation()
            ),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn elicitation_skip_cancel_keys_gated_on_pending() {
        // s / c resolve a pending elicitation from transcript focus.
        assert_eq!(
            dispatch(
                Focus::Transcript,
                &key(KeyCode::Char('s')),
                ctx_pending_elicitation()
            ),
            Intent::SkipElicitation
        );
        assert_eq!(
            dispatch(
                Focus::Transcript,
                &key(KeyCode::Char('c')),
                ctx_pending_elicitation()
            ),
            Intent::CancelElicitation
        );
        // Without a pending elicitation, s / c are not elicitation intents
        // (c falls through to Ignore, s likewise) so they stay free.
        assert!(!matches!(
            dispatch(Focus::Transcript, &key(KeyCode::Char('s')), ctx()),
            Intent::SkipElicitation
        ));
        // From the composer they must type, never resolve.
        assert!(matches!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Char('s')),
                ctx_pending_elicitation()
            ),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn esc_in_composer_is_native_interrupt_not_exit() {
        // Idle: Esc is an inert no-op (never drops out of the view).
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Esc), ctx()),
            Intent::Ignore
        );
        // Generating: Esc interrupts the turn, like a native agent.
        let busy = InputContext {
            agent_busy: true,
            ..InputContext::default()
        };
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Esc), busy),
            Intent::CancelInFlight
        );
    }

    #[test]
    fn ctrl_q_exits_from_any_focus() {
        // Ctrl-Q is the single way out, mirroring live-send's exit chord.
        for focus in [Focus::Composer, Focus::Transcript, Focus::Approval] {
            assert_eq!(
                dispatch(
                    focus,
                    &key_mod(KeyCode::Char('q'), KeyModifiers::CONTROL),
                    ctx_pending()
                ),
                Intent::Exit
            );
        }
        // Plain 'q' in the composer is still a typed character.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('q')), ctx()),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn esc_from_transcript_returns_to_composer() {
        // The transcript is a secondary focus; Esc backs out to the
        // composer rather than leaving the view.
        let intent = dispatch(Focus::Transcript, &key(KeyCode::Esc), ctx());
        assert_eq!(intent, Intent::SetFocus(Focus::Composer));
    }

    #[test]
    fn page_keys_scroll_transcript_from_composer() {
        // Reading history never requires leaving the composer.
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::PageUp), ctx()),
            Intent::Scroll(-PAGE_SCROLL_LINES)
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::PageDown), ctx()),
            Intent::Scroll(PAGE_SCROLL_LINES)
        );
    }

    #[test]
    fn tab_is_inert_and_shift_tab_opens_mode_picker() {
        // There is no Tab-to-the-chat toggle anymore: plain Tab is inert.
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Tab), ctx()),
            Intent::Ignore
        );
        // Shift+Tab (crossterm BackTab) opens the mode picker when the
        // agent advertised modes, mirroring Claude Code's mode chord.
        let with_modes = InputContext {
            has_modes: true,
            ..InputContext::default()
        };
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::BackTab), with_modes),
            Intent::OpenModePicker
        );
        // Without modes it's inert (nothing to pick).
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::BackTab), ctx()),
            Intent::Ignore
        );
    }

    #[test]
    fn esc_from_approval_interrupts() {
        // The approval is modal (it grabbed focus), so Esc interrupts the
        // turn, which cancels the request and hands focus back.
        let intent = dispatch(Focus::Approval, &key(KeyCode::Esc), ctx_pending());
        assert_eq!(intent, Intent::CancelInFlight);
    }

    #[test]
    fn ctrl_c_cancels_from_any_focus() {
        for focus in [Focus::Composer, Focus::Transcript, Focus::Approval] {
            let intent = dispatch(
                focus,
                &key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
                ctx_pending(),
            );
            assert_eq!(intent, Intent::CancelInFlight);
        }
    }

    #[test]
    fn ctrl_x_clears_queue_from_any_focus() {
        for focus in [Focus::Composer, Focus::Transcript, Focus::Approval] {
            let intent = dispatch(
                focus,
                &key_mod(KeyCode::Char('x'), KeyModifiers::CONTROL),
                ctx(),
            );
            assert_eq!(intent, Intent::ClearQueue);
        }
        // Plain 'x' in the composer is still a typed character.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('x')), ctx()),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn plain_o_opens_browser_only_from_transcript() {
        // Composer focus must pass through.
        let composer = dispatch(Focus::Composer, &key(KeyCode::Char('o')), ctx());
        assert!(matches!(composer, Intent::Compose(_)));
        // Transcript focus opens browser.
        let transcript = dispatch(Focus::Transcript, &key(KeyCode::Char('o')), ctx());
        assert_eq!(transcript, Intent::OpenInBrowser);
    }

    #[test]
    fn enter_in_composer_submits() {
        let intent = dispatch(Focus::Composer, &key(KeyCode::Enter), ctx());
        assert_eq!(intent, Intent::SubmitPrompt);
    }

    #[test]
    fn shift_enter_in_composer_inserts_newline() {
        let intent = dispatch(
            Focus::Composer,
            &key_mod(KeyCode::Enter, KeyModifiers::SHIFT),
            ctx(),
        );
        assert!(matches!(intent, Intent::Compose(_)));
    }

    #[test]
    fn ctrl_j_in_composer_inserts_newline() {
        // A bare line feed (\n) decodes to Ctrl+J in raw mode; some terminals
        // send it for Shift+Enter (e.g. Ghostty `shift+enter=text:\n`). It must
        // forward a plain Enter so the textarea inserts a newline rather than
        // running its default Ctrl+J delete-to-line-head binding.
        let intent = dispatch(
            Focus::Composer,
            &key_mod(KeyCode::Char('j'), KeyModifiers::CONTROL),
            ctx(),
        );
        assert_eq!(
            intent,
            Intent::Compose(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        );
    }

    #[test]
    fn tab_from_transcript_routes_to_approval_when_pending() {
        let with_pending = dispatch(Focus::Transcript, &key(KeyCode::Tab), ctx_pending());
        assert_eq!(with_pending, Intent::SetFocus(Focus::Approval));
        let without = dispatch(Focus::Transcript, &key(KeyCode::Tab), ctx());
        assert_eq!(without, Intent::SetFocus(Focus::Composer));
    }

    #[test]
    fn vim_scroll_keys_only_active_in_transcript() {
        assert_eq!(
            dispatch(Focus::Transcript, &key(KeyCode::Char('j')), ctx()),
            Intent::Scroll(1)
        );
        // 'j' in composer is a typed character, not a scroll.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('j')), ctx()),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn picker_open_claims_navigation_and_accept_keys() {
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Down), ctx_picker()),
            Intent::SlashMove(1)
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Up), ctx_picker()),
            Intent::SlashMove(-1)
        );
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL),
                ctx_picker()
            ),
            Intent::SlashMove(1)
        );
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL),
                ctx_picker()
            ),
            Intent::SlashMove(-1)
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Enter), ctx_picker()),
            Intent::SlashAccept
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Tab), ctx_picker()),
            Intent::SlashAccept
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Esc), ctx_picker()),
            Intent::SlashDismiss
        );
    }

    #[test]
    fn picker_open_still_passes_typed_chars_through() {
        // Typing a letter while the picker is open narrows the query;
        // it must NOT be stolen as a picker command.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('a')), ctx_picker()),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn picker_closed_enter_still_submits() {
        // Focus-isolation regression: with the picker closed, Enter must
        // submit even if an approval is pending.
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Enter), ctx_pending()),
            Intent::SubmitPrompt
        );
    }

    #[test]
    fn mention_picker_routes_navigation_keys() {
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Down), ctx_mention()),
            Intent::MentionNavigate(1)
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Up), ctx_mention()),
            Intent::MentionNavigate(-1)
        );
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key_mod(KeyCode::Char('n'), KeyModifiers::CONTROL),
                ctx_mention()
            ),
            Intent::MentionNavigate(1)
        );
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key_mod(KeyCode::Char('p'), KeyModifiers::CONTROL),
                ctx_mention()
            ),
            Intent::MentionNavigate(-1)
        );
    }

    #[test]
    fn mention_picker_enter_and_tab_accept() {
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Enter), ctx_mention()),
            Intent::MentionAccept
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Tab), ctx_mention()),
            Intent::MentionAccept
        );
    }

    #[test]
    fn mention_picker_esc_closes_not_focus() {
        // With the picker open, Esc closes it; with it closed, Esc moves
        // focus to the transcript as usual.
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Esc), ctx_mention()),
            Intent::MentionClose
        );
        // With the picker closed and idle, Esc is an inert no-op.
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Esc), ctx()),
            Intent::Ignore
        );
    }

    #[test]
    fn arrow_up_recalls_when_caret_at_origin_with_a_queue() {
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Up),
                ctx_recall(true, false, 2)
            ),
            Intent::RecallQueued(-1)
        );
    }

    #[test]
    fn arrow_up_moves_caret_when_not_at_origin() {
        // Multi-line draft, caret mid-text: Up is normal caret movement,
        // never a recall, even with a non-empty queue.
        assert!(matches!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Up),
                ctx_recall(false, false, 2)
            ),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn arrow_up_does_not_recall_with_empty_queue() {
        assert!(matches!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Up),
                ctx_recall(true, false, 0)
            ),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn arrows_navigate_queue_while_browsing_regardless_of_caret() {
        // Once browsing, both arrows own navigation even though the caret
        // is not at the origin (it sits at the end of the loaded prompt).
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Up),
                ctx_recall(false, true, 2)
            ),
            Intent::RecallQueued(-1)
        );
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Down),
                ctx_recall(false, true, 2)
            ),
            Intent::RecallQueued(1)
        );
    }

    #[test]
    fn recall_keys_win_over_open_picker_while_browsing() {
        // A recalled prompt that looks like a slash query opens the picker;
        // recall navigation must still own Up/Down/Esc/Enter.
        let ctx = InputContext {
            slash_picker_open: true,
            browsing_queue: true,
            queue_len: 2,
            ..InputContext::default()
        };
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Up), ctx),
            Intent::RecallQueued(-1)
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Down), ctx),
            Intent::RecallQueued(1)
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Esc), ctx),
            Intent::RecallCancel
        );
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Enter), ctx),
            Intent::SubmitPrompt
        );
        // Typed characters still fall through to narrow the picker.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('a')), ctx),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn esc_while_browsing_cancels_recall() {
        assert_eq!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Esc),
                ctx_recall(false, true, 2)
            ),
            Intent::RecallCancel
        );
        // Not browsing and idle: Esc is an inert no-op (Ctrl-Q exits).
        assert_eq!(
            dispatch(Focus::Composer, &key(KeyCode::Esc), ctx()),
            Intent::Ignore
        );
    }

    #[test]
    fn arrow_down_does_not_recall_when_not_browsing() {
        assert!(matches!(
            dispatch(
                Focus::Composer,
                &key(KeyCode::Down),
                ctx_recall(true, false, 2)
            ),
            Intent::Compose(_)
        ));
    }

    use ratatui::layout::Rect;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn layout() -> ViewLayout {
        // 80x24 frame: transcript rows 0-19, status row 20, no queue,
        // composer rows 21-23.
        ViewLayout {
            transcript: Rect::new(0, 0, 80, 20),
            status: Rect::new(0, 20, 80, 1),
            queue: Rect::new(0, 21, 80, 0),
            composer: Rect::new(0, 21, 80, 3),
        }
    }

    #[test]
    fn wheel_scrolls_transcript_regardless_of_pointer_pane() {
        let l = layout();
        // Over the transcript.
        assert_eq!(
            dispatch_mouse(&mouse(MouseEventKind::ScrollUp, 5, 5), Some(&l)),
            Intent::Scroll(-WHEEL_SCROLL_LINES)
        );
        // Over the composer: still scrolls the transcript.
        assert_eq!(
            dispatch_mouse(&mouse(MouseEventKind::ScrollDown, 5, 22), Some(&l)),
            Intent::Scroll(WHEEL_SCROLL_LINES)
        );
        // Even with no layout yet (wheel needs no hit-test).
        assert_eq!(
            dispatch_mouse(&mouse(MouseEventKind::ScrollUp, 0, 0), None),
            Intent::Scroll(-WHEEL_SCROLL_LINES)
        );
    }

    #[test]
    fn left_click_always_focuses_composer() {
        // One pane: a click anywhere (transcript, composer, status row)
        // focuses the composer, so reading the chat never drops you into
        // a separate mode you'd have to click back out of.
        let l = layout();
        for row in [3u16, 20, 22] {
            assert_eq!(
                dispatch_mouse(
                    &mouse(MouseEventKind::Down(MouseButton::Left), 10, row),
                    Some(&l)
                ),
                Intent::SetFocus(Focus::Composer),
                "click at row {row}"
            );
        }
    }

    #[test]
    fn click_before_first_draw_focuses_composer() {
        // No layout yet: a click still just focuses the composer (there
        // is only one focus target), no hit-test needed.
        assert_eq!(
            dispatch_mouse(
                &mouse(MouseEventKind::Down(MouseButton::Left), 10, 10),
                None
            ),
            Intent::SetFocus(Focus::Composer)
        );
    }

    #[test]
    fn non_left_mouse_events_are_ignored() {
        let l = layout();
        for kind in [
            MouseEventKind::Down(MouseButton::Right),
            MouseEventKind::Down(MouseButton::Middle),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
        ] {
            assert_eq!(
                dispatch_mouse(&mouse(kind, 5, 5), Some(&l)),
                Intent::Ignore,
                "{kind:?}"
            );
        }
    }

    #[test]
    fn mention_picker_passes_typed_chars_through() {
        // Typing narrows the query; Backspace edits the textarea. Neither
        // is stolen by the picker.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('s')), ctx_mention()),
            Intent::Compose(_)
        ));
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Backspace), ctx_mention()),
            Intent::Compose(_)
        ));
    }
}
