//! Focus model + key dispatch for the structured view.
//!
//! The composer is the home base: the view lands there so you can type
//! immediately, and reading history never needs a focus switch (wheel and
//! `PageUp`/`PageDown` scroll the transcript from the composer). `Ctrl-Q`
//! leaves, `Esc` is an agent-style interrupt rather than an exit, and `Tab`
//! reaches the transcript for its power keys. The composer captures every typed
//! key, including `a`/`A`/`d`, so typing "always allow" into a prompt never
//! resolves an approval; a pending approval opens a modal shelf that does
//! accept those keys.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

use super::state::ViewLayout;
use crate::acp::protocol::ApprovalDecisionWire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Composer,
    Transcript,
    Approval,
    /// The plugin pane panel is open (#2467). A modal read-only overlay: it
    /// owns the keyboard while up, scrolls with the transcript keys, and
    /// closes back to `Transcript`. Opened with `p` from the transcript.
    Pane,
}

/// What the input dispatcher decided to do with this key. The view layer runs
/// the side effects, so input.rs stays a pure translator.
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
    /// Browse the prompt queue shell-history style: negative toward older
    /// entries (ArrowUp), positive toward newer. The view loads the entry into
    /// the composer for editing.
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
    /// Open the answer picker for the oldest pending elicitation (transcript
    /// `a`). The view decides whether the form is natively answerable.
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

/// Ambient state the dispatcher needs beyond the raw key: whether an approval
/// is pending (gates Tab routing) and whether the slash or `@`-mention picker is
/// open (each claims navigation keys in the composer).
#[derive(Debug, Clone, Copy, Default)]
pub struct InputContext {
    pub has_pending_approval: bool,
    /// A pending `AskUserQuestion` elicitation. Gates the transcript-focus
    /// skip/cancel keys; the answer form itself is web-only.
    pub has_pending_elicitation: bool,
    pub slash_picker_open: bool,
    pub mention_picker_open: bool,
    /// Composer caret is at row 0, col 0. Gates ArrowUp entry into queue-recall
    /// so multi-line caret movement keeps working until the top-left.
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
    /// The open choice picker is numbered (the plugin-link picker), so `1`-`9`
    /// pick and accept a row directly. Off for mode / elicitation pickers.
    pub choice_numbered: bool,
    /// The agent advertised permission modes; gates the transcript `m` key.
    pub has_modes: bool,
    /// The agent is generating. Gates `Esc` in the composer: it interrupts the
    /// turn while busy, and is an inert no-op when idle.
    pub agent_busy: bool,
}

/// Translate a key event into an [`Intent`] for the current focus. Pure, so the
/// whole focus model is unit-testable without a ratatui surface.
pub fn dispatch(focus: Focus, key: &KeyEvent, ctx: InputContext) -> Intent {
    // Universal: Ctrl-C cancels any in-flight prompt. Deliberately not an exit:
    // the reflex from a tmux session is "stop the agent, don't quit the screen".
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Intent::CancelInFlight;
    }
    // Universal: Ctrl-q leaves the view from any focus, mirroring live-send's
    // exit chord.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
        return Intent::Exit;
    }
    // Universal: Ctrl-o opens the browser. `o` alone is transcript-only, so
    // typing "no" into the composer doesn't open a tab.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
        return Intent::OpenInBrowser;
    }
    // Universal: Ctrl-x drops every queued prompt, intercepted before the
    // composer sees it so a backlog can always be abandoned. No-op when empty.
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('x') {
        return Intent::ClearQueue;
    }
    // An open choice picker owns its navigation keys from any focus: the user
    // opened it deliberately and it closes on Enter/Esc.
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
        Focus::Pane => pane_keys(key),
    }
}

/// Transcript lines scrolled per mouse-wheel tick, matching the home
/// screen's preview wheel step.
const WHEEL_SCROLL_LINES: i32 = 3;

/// Transcript lines scrolled per `PageUp`/`PageDown`, from either the
/// transcript or the composer.
const PAGE_SCROLL_LINES: i32 = 10;

/// Translate a mouse event into an [`Intent`]. The wheel always scrolls the
/// focused scrollback, whatever pane the pointer is over; a left click moves
/// focus to the pane under it. `layout` is the last-drawn frame's geometry, so
/// before the first draw clicks are ignored. While the modal plugin pane overlay
/// is up (#2467) clicks are swallowed rather than routed to a hidden pane.
pub fn dispatch_mouse(mouse: &MouseEvent, focus: Focus, layout: Option<&ViewLayout>) -> Intent {
    match mouse.kind {
        MouseEventKind::ScrollUp => Intent::Scroll(-WHEEL_SCROLL_LINES),
        MouseEventKind::ScrollDown => Intent::Scroll(WHEEL_SCROLL_LINES),
        MouseEventKind::Down(MouseButton::Left) if matches!(focus, Focus::Pane) => Intent::Ignore,
        MouseEventKind::Down(MouseButton::Left) => match layout {
            Some(layout) if layout.approval.contains((mouse.column, mouse.row).into()) => {
                Intent::SetFocus(Focus::Approval)
            }
            Some(layout) if layout.composer.contains((mouse.column, mouse.row).into()) => {
                Intent::SetFocus(Focus::Composer)
            }
            Some(layout) if layout.transcript.contains((mouse.column, mouse.row).into()) => {
                Intent::SetFocus(Focus::Transcript)
            }
            _ => Intent::Ignore,
        },
        _ => Intent::Ignore,
    }
}

fn composer_keys(key: &KeyEvent, ctx: InputContext) -> Intent {
    let slash_picker_open = ctx.slash_picker_open;
    let mention_picker_open = ctx.mention_picker_open;
    // While browsing the queue, recall navigation owns its core keys even when
    // the recalled text would open the slash / `@` picker (e.g. a queued
    // "/clear"). Typed characters still fall through to narrow the picker.
    if ctx.browsing_queue {
        match (key.modifiers, key.code) {
            (m, KeyCode::Up) if m.is_empty() => return Intent::RecallQueued(-1),
            (m, KeyCode::Down) if m.is_empty() => return Intent::RecallQueued(1),
            (m, KeyCode::Esc) if m.is_empty() => return Intent::RecallCancel,
            (m, KeyCode::Enter) if m.is_empty() => return Intent::SubmitPrompt,
            _ => {}
        }
    }
    // An open picker claims navigation + accept/dismiss keys; everything else
    // falls through to the composer rules below. Slash and mention pickers are
    // mutually exclusive, but slash wins the tie defensively.
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
        // Queue recall: ArrowUp browses older entries when already browsing, or
        // when the caret is top-left and the queue is non-empty; ArrowDown walks
        // back only while browsing. Otherwise they are normal caret movement.
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
        // Ctrl+J is crossterm's raw-mode decoding of a bare line feed, which
        // some terminals send for Shift+Enter. Forward a plain Enter: the raw
        // Ctrl+J would hit the textarea's delete-to-line-head binding.
        (m, KeyCode::Char('j')) if m == KeyModifiers::CONTROL => {
            Intent::Compose(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        }
        // Page keys scroll the transcript without leaving the composer. They
        // are not textarea editing keys, so nothing is lost by claiming them.
        (m, KeyCode::PageUp) if m.is_empty() => Intent::Scroll(-PAGE_SCROLL_LINES),
        (m, KeyCode::PageDown) if m.is_empty() => Intent::Scroll(PAGE_SCROLL_LINES),
        // Esc is native-agent behavior, not an exit (Ctrl-Q leaves): it
        // interrupts a generating turn and is an inert no-op when idle, so a
        // stray Esc never drops you out. Pickers intercept Esc above.
        (m, KeyCode::Esc) if m.is_empty() => {
            if ctx.agent_busy {
                Intent::CancelInFlight
            } else {
                Intent::Ignore
            }
        }
        // Shift+Tab (crossterm BackTab) opens the permission-mode picker,
        // mirroring Claude Code's mode-cycle chord; a no-op when the agent
        // advertised none. Plain Tab is inert (pickers claim it to accept).
        (_, KeyCode::BackTab) if ctx.has_modes => Intent::OpenModePicker,
        // BackTab is never text; swallow it so it can't leak into the composer.
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
        // Esc backs out one level to the composer (the home base) rather than
        // leaving the view.
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
        // Plain 'p' opens the plugin pane panel. Transcript-only (like 'o'),
        // so typing 'p' in the composer never opens it.
        (m, KeyCode::Char('p')) if m.is_empty() => Intent::SetFocus(Focus::Pane),
        _ => Intent::Ignore,
    }
}

/// Keys while the plugin pane panel is open. Scrolls with the transcript
/// vocabulary; `Esc` / `p` close it, `Tab` jumps to the composer. The universal
/// `Ctrl-c/o/x` chords still apply.
fn pane_keys(key: &KeyEvent) -> Intent {
    match (key.modifiers, key.code) {
        (m, KeyCode::Esc) if m.is_empty() => Intent::SetFocus(Focus::Transcript),
        (m, KeyCode::Char('p')) if m.is_empty() => Intent::SetFocus(Focus::Transcript),
        (m, KeyCode::Tab) if m.is_empty() => Intent::SetFocus(Focus::Composer),
        (m, KeyCode::Char('j')) if m.is_empty() => Intent::Scroll(1),
        (m, KeyCode::Char('k')) if m.is_empty() => Intent::Scroll(-1),
        (m, KeyCode::Down) if m.is_empty() => Intent::Scroll(1),
        (m, KeyCode::Up) if m.is_empty() => Intent::Scroll(-1),
        (m, KeyCode::PageDown) if m.is_empty() => Intent::Scroll(PAGE_SCROLL_LINES),
        (m, KeyCode::PageUp) if m.is_empty() => Intent::Scroll(-PAGE_SCROLL_LINES),
        (m, KeyCode::Char('g')) if m.is_empty() => Intent::Scroll(i32::MIN),
        (m, KeyCode::Char('G')) if m.contains(KeyModifiers::SHIFT) => Intent::Scroll(i32::MAX),
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
        // A pending approval is modal, so Esc interrupts the turn: cancelling
        // clears the request and drops back to the composer.
        (m, KeyCode::Esc) if m.is_empty() => Intent::CancelInFlight,
        _ => Intent::Ignore,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;
    use KeyCode::{BackTab, Backspace, Char, Down, Enter, Esc, PageDown, PageUp, Tab, Up};

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const CTRL: KeyModifiers = KeyModifiers::CONTROL;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const ANY_FOCUS: [Focus; 3] = [Focus::Composer, Focus::Transcript, Focus::Approval];

    /// Expected dispatch outcome. `Composes` matches any `Intent::Compose`, whose
    /// payload is the forwarded key event.
    #[derive(Debug)]
    enum Want {
        Is(Intent),
        Composes,
    }
    use Want::{Composes, Is};

    fn check(focus: Focus, code: KeyCode, mods: KeyModifiers, ctx: InputContext, want: Want) {
        let got = dispatch(focus, &KeyEvent::new(code, mods), ctx);
        let ok = match &want {
            Want::Is(expected) => got == *expected,
            Want::Composes => matches!(got, Intent::Compose(_)),
        };
        assert!(
            ok,
            "{focus:?} {code:?} {mods:?} {ctx:?} -> {got:?}, want {want:?}"
        );
    }

    fn ctx() -> InputContext {
        InputContext::default()
    }

    fn ctx_pending() -> InputContext {
        InputContext {
            has_pending_approval: true,
            ..InputContext::default()
        }
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

    fn ctx_elicitation() -> InputContext {
        InputContext {
            has_pending_elicitation: true,
            ..InputContext::default()
        }
    }

    fn ctx_modes() -> InputContext {
        InputContext {
            has_modes: true,
            ..InputContext::default()
        }
    }

    fn ctx_choice(numbered: bool) -> InputContext {
        InputContext {
            choice_picker_open: true,
            choice_numbered: numbered,
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

    /// Chords that work from every focus, so "stop the agent", "abandon the
    /// queue" and "get me out" never depend on where the keyboard is.
    #[test]
    fn universal_chords_apply_from_any_focus() {
        for focus in ANY_FOCUS.into_iter().chain([Focus::Pane]) {
            check(
                focus,
                Char('c'),
                CTRL,
                ctx_pending(),
                Is(Intent::CancelInFlight),
            );
            check(focus, Char('x'), CTRL, ctx(), Is(Intent::ClearQueue));
            check(focus, Char('q'), CTRL, ctx_pending(), Is(Intent::Exit));
        }
        // Unmodified, the same letters are ordinary composer text.
        for ch in "cxqo".chars() {
            check(Focus::Composer, Char(ch), NONE, ctx(), Composes);
        }
    }

    /// The composer captures every typed key, so typing "always allow" into a
    /// prompt cannot resolve a pending approval; only approval focus does.
    #[test]
    fn approval_letters_resolve_only_under_approval_focus() {
        for ch in "always allow deny".chars() {
            check(Focus::Composer, Char(ch), NONE, ctx_pending(), Composes);
        }
        for (ch, mods) in [('a', NONE), ('A', SHIFT), ('d', NONE)] {
            let got = dispatch(
                Focus::Transcript,
                &KeyEvent::new(Char(ch), mods),
                ctx_pending(),
            );
            assert!(
                !matches!(got, Intent::ResolveApproval(_)),
                "{ch} resolved from transcript focus: {got:?}"
            );
        }
        for (ch, mods, decision) in [
            ('a', NONE, ApprovalDecisionWire::Allow),
            ('A', SHIFT, ApprovalDecisionWire::AllowAlways),
            ('d', NONE, ApprovalDecisionWire::Deny),
        ] {
            let want = Is(Intent::ResolveApproval(decision));
            check(Focus::Approval, Char(ch), mods, ctx_pending(), want);
        }
    }

    /// An open choice picker owns navigation and accept keys from any focus, and
    /// swallows everything else so a stray `a` cannot resolve the approval under
    /// it. Digits pick a row only on a numbered (plugin-link) picker.
    #[test]
    fn choice_picker_owns_its_keys() {
        for focus in ANY_FOCUS {
            for (code, want) in [
                (Down, Intent::ChoiceNavigate(1)),
                (Up, Intent::ChoiceNavigate(-1)),
                (Enter, Intent::ChoiceAccept),
                (Esc, Intent::ChoiceCancel),
                (Char('a'), Intent::Ignore),
            ] {
                check(focus, code, NONE, ctx_choice(false), Is(want));
            }
        }
        check(
            Focus::Composer,
            Char('1'),
            NONE,
            ctx_choice(true),
            Is(Intent::ChoicePick(0)),
        );
        check(
            Focus::Transcript,
            Char('3'),
            NONE,
            ctx_choice(true),
            Is(Intent::ChoicePick(2)),
        );
        check(
            Focus::Composer,
            Enter,
            NONE,
            ctx_choice(true),
            Is(Intent::ChoiceAccept),
        );
        check(
            Focus::Composer,
            Char('1'),
            NONE,
            ctx_choice(false),
            Is(Intent::Ignore),
        );
    }

    /// Transcript power keys stay out of the composer, where the same letters are
    /// text. Each row is (focus, key, modifiers, context, expected).
    #[test]
    fn focus_decides_what_a_bare_key_means() {
        let cases = [
            (
                Focus::Transcript,
                Char('p'),
                NONE,
                ctx(),
                Is(Intent::SetFocus(Focus::Pane)),
            ),
            (Focus::Composer, Char('p'), NONE, ctx(), Composes),
            (
                Focus::Transcript,
                Char('j'),
                NONE,
                ctx(),
                Is(Intent::Scroll(1)),
            ),
            (Focus::Composer, Char('j'), NONE, ctx(), Composes),
            (
                Focus::Transcript,
                Char('o'),
                NONE,
                ctx(),
                Is(Intent::OpenInBrowser),
            ),
            (
                Focus::Transcript,
                Char('m'),
                NONE,
                ctx_modes(),
                Is(Intent::OpenModePicker),
            ),
            (
                Focus::Transcript,
                Char('m'),
                NONE,
                ctx(),
                Is(Intent::Ignore),
            ),
            (Focus::Composer, Char('m'), NONE, ctx_modes(), Composes),
            (
                Focus::Transcript,
                Char('a'),
                NONE,
                ctx_elicitation(),
                Is(Intent::AnswerElicitation),
            ),
            (
                Focus::Transcript,
                Char('a'),
                NONE,
                ctx(),
                Is(Intent::Ignore),
            ),
            (
                Focus::Composer,
                Char('a'),
                NONE,
                ctx_elicitation(),
                Composes,
            ),
            (
                Focus::Transcript,
                Char('s'),
                NONE,
                ctx_elicitation(),
                Is(Intent::SkipElicitation),
            ),
            (
                Focus::Transcript,
                Char('c'),
                NONE,
                ctx_elicitation(),
                Is(Intent::CancelElicitation),
            ),
            (
                Focus::Transcript,
                Char('s'),
                NONE,
                ctx(),
                Is(Intent::Ignore),
            ),
            (
                Focus::Composer,
                Char('s'),
                NONE,
                ctx_elicitation(),
                Composes,
            ),
        ];
        for (focus, code, mods, context, want) in cases {
            check(focus, code, mods, context, want);
        }
    }

    /// The plugin pane overlay is modal: it scrolls with the transcript
    /// vocabulary and closes back to the transcript.
    #[test]
    fn pane_overlay_scrolls_and_closes() {
        for (code, mods, want) in [
            (Char('j'), NONE, Intent::Scroll(1)),
            (Up, NONE, Intent::Scroll(-1)),
            (Char('G'), SHIFT, Intent::Scroll(i32::MAX)),
            (Esc, NONE, Intent::SetFocus(Focus::Transcript)),
            (Char('p'), NONE, Intent::SetFocus(Focus::Transcript)),
            (Tab, NONE, Intent::SetFocus(Focus::Composer)),
        ] {
            check(Focus::Pane, code, mods, ctx(), Is(want));
        }
    }

    /// `Esc` is an agent-style interrupt, never an exit: it cancels a generating
    /// turn, backs the transcript out one level, and is inert when idle.
    #[test]
    fn esc_interrupts_or_backs_out_but_never_exits() {
        let busy = InputContext {
            agent_busy: true,
            ..InputContext::default()
        };
        check(Focus::Composer, Esc, NONE, ctx(), Is(Intent::Ignore));
        check(Focus::Composer, Esc, NONE, busy, Is(Intent::CancelInFlight));
        check(
            Focus::Transcript,
            Esc,
            NONE,
            ctx(),
            Is(Intent::SetFocus(Focus::Composer)),
        );
        check(
            Focus::Approval,
            Esc,
            NONE,
            ctx_pending(),
            Is(Intent::CancelInFlight),
        );
    }

    /// Reading history and submitting never need a focus switch; `Tab` routes to
    /// a pending approval, and Shift+Tab (BackTab) opens the mode picker.
    #[test]
    fn composer_keys_scroll_submit_and_route() {
        let cases = [
            (PageUp, NONE, ctx(), Is(Intent::Scroll(-PAGE_SCROLL_LINES))),
            (PageDown, NONE, ctx(), Is(Intent::Scroll(PAGE_SCROLL_LINES))),
            (Enter, NONE, ctx(), Is(Intent::SubmitPrompt)),
            (Enter, NONE, ctx_pending(), Is(Intent::SubmitPrompt)),
            (Enter, SHIFT, ctx(), Composes),
            (Tab, NONE, ctx(), Is(Intent::Ignore)),
            (BackTab, NONE, ctx_modes(), Is(Intent::OpenModePicker)),
            (BackTab, NONE, ctx(), Is(Intent::Ignore)),
        ];
        for (code, mods, context, want) in cases {
            check(Focus::Composer, code, mods, context, want);
        }
        check(
            Focus::Transcript,
            Tab,
            NONE,
            ctx_pending(),
            Is(Intent::SetFocus(Focus::Approval)),
        );
        check(
            Focus::Transcript,
            Tab,
            NONE,
            ctx(),
            Is(Intent::SetFocus(Focus::Composer)),
        );
    }

    /// A bare line feed decodes to Ctrl+J in raw mode, and some terminals send it
    /// for Shift+Enter. It must forward a plain Enter so the textarea inserts a
    /// newline instead of running its delete-to-line-head binding.
    #[test]
    fn ctrl_j_forwards_a_plain_enter() {
        let got = dispatch(Focus::Composer, &KeyEvent::new(Char('j'), CTRL), ctx());
        assert_eq!(got, Intent::Compose(KeyEvent::new(Enter, NONE)));
    }

    /// The slash and `@`-mention pickers claim navigation and accept keys while
    /// open; typed characters still fall through to narrow the query.
    #[test]
    fn composer_pickers_claim_navigation_only() {
        let slash = [
            (Down, NONE, Intent::SlashMove(1)),
            (Up, NONE, Intent::SlashMove(-1)),
            (Char('n'), CTRL, Intent::SlashMove(1)),
            (Char('p'), CTRL, Intent::SlashMove(-1)),
            (Enter, NONE, Intent::SlashAccept),
            (Tab, NONE, Intent::SlashAccept),
            (Esc, NONE, Intent::SlashDismiss),
        ];
        for (code, mods, want) in slash {
            check(Focus::Composer, code, mods, ctx_picker(), Is(want));
        }
        let mention = [
            (Down, NONE, Intent::MentionNavigate(1)),
            (Up, NONE, Intent::MentionNavigate(-1)),
            (Char('n'), CTRL, Intent::MentionNavigate(1)),
            (Char('p'), CTRL, Intent::MentionNavigate(-1)),
            (Enter, NONE, Intent::MentionAccept),
            (Tab, NONE, Intent::MentionAccept),
            (Esc, NONE, Intent::MentionClose),
        ];
        for (code, mods, want) in mention {
            check(Focus::Composer, code, mods, ctx_mention(), Is(want));
        }
        check(Focus::Composer, Char('a'), NONE, ctx_picker(), Composes);
        check(Focus::Composer, Char('s'), NONE, ctx_mention(), Composes);
        check(Focus::Composer, Backspace, NONE, ctx_mention(), Composes);
    }

    /// Queue recall behaves like shell history: ArrowUp enters it only from the
    /// caret origin with something to recall, and once browsing both arrows own
    /// navigation whatever the caret position or an open picker.
    #[test]
    fn arrow_keys_recall_the_queue_only_when_eligible() {
        let browsing_with_picker = InputContext {
            slash_picker_open: true,
            browsing_queue: true,
            queue_len: 2,
            ..InputContext::default()
        };
        let cases = [
            (Up, ctx_recall(true, false, 2), Is(Intent::RecallQueued(-1))),
            (Up, ctx_recall(false, false, 2), Composes),
            (Up, ctx_recall(true, false, 0), Composes),
            (Down, ctx_recall(true, false, 2), Composes),
            (Up, ctx_recall(false, true, 2), Is(Intent::RecallQueued(-1))),
            (
                Down,
                ctx_recall(false, true, 2),
                Is(Intent::RecallQueued(1)),
            ),
            (Esc, ctx_recall(false, true, 2), Is(Intent::RecallCancel)),
            (Up, browsing_with_picker, Is(Intent::RecallQueued(-1))),
            (Down, browsing_with_picker, Is(Intent::RecallQueued(1))),
            (Esc, browsing_with_picker, Is(Intent::RecallCancel)),
            (Enter, browsing_with_picker, Is(Intent::SubmitPrompt)),
            (Char('a'), browsing_with_picker, Composes),
        ];
        for (code, context, want) in cases {
            check(Focus::Composer, code, NONE, context, want);
        }
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    /// 80x24 frame: transcript rows 0-19, status row 20, composer rows 21-23.
    fn layout() -> ViewLayout {
        ViewLayout {
            transcript: Rect::new(0, 0, 80, 20),
            status: Rect::new(0, 20, 80, 1),
            approval: Rect::new(0, 21, 80, 0),
            queue: Rect::new(0, 21, 80, 0),
            composer: Rect::new(0, 21, 80, 3),
        }
    }

    /// The wheel scrolls the focused scrollback wherever the pointer is and needs
    /// no hit-test, so it works before the first draw too.
    #[test]
    fn wheel_scrolls_transcript_regardless_of_pointer_pane() {
        let l = layout();
        let cases = [
            (
                MouseEventKind::ScrollUp,
                5,
                5,
                Some(&l),
                -WHEEL_SCROLL_LINES,
            ),
            (
                MouseEventKind::ScrollDown,
                5,
                22,
                Some(&l),
                WHEEL_SCROLL_LINES,
            ),
            (MouseEventKind::ScrollUp, 0, 0, None, -WHEEL_SCROLL_LINES),
        ];
        for (kind, col, row, layout, delta) in cases {
            let got = dispatch_mouse(&mouse(kind, col, row), Focus::Transcript, layout);
            assert_eq!(got, Intent::Scroll(delta), "{kind:?} at {col},{row}");
        }
    }

    /// A left click focuses the pane under the pointer. Before the first draw
    /// there is no geometry to hit-test, and while the modal pane overlay is up
    /// (#2467) clicks must not focus a pane the user cannot see.
    #[test]
    fn left_click_focuses_the_visible_pane_under_the_pointer() {
        let l = layout();
        let click = MouseEventKind::Down(MouseButton::Left);
        let cases = [
            (
                10,
                3,
                Focus::Transcript,
                Some(&l),
                Intent::SetFocus(Focus::Transcript),
            ),
            (
                10,
                22,
                Focus::Transcript,
                Some(&l),
                Intent::SetFocus(Focus::Composer),
            ),
            (10, 20, Focus::Transcript, Some(&l), Intent::Ignore),
            (10, 10, Focus::Transcript, None, Intent::Ignore),
            (10, 22, Focus::Pane, Some(&l), Intent::Ignore),
        ];
        for (col, row, focus, layout, want) in cases {
            let got = dispatch_mouse(&mouse(click, col, row), focus, layout);
            assert_eq!(got, want, "click at {col},{row} under {focus:?}");
        }
        // The overlay still lets the wheel through; the view routes the delta.
        let got = dispatch_mouse(
            &mouse(MouseEventKind::ScrollDown, 10, 5),
            Focus::Pane,
            Some(&l),
        );
        assert_eq!(got, Intent::Scroll(WHEEL_SCROLL_LINES));
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
                dispatch_mouse(&mouse(kind, 5, 5), Focus::Transcript, Some(&l)),
                Intent::Ignore,
                "{kind:?}"
            );
        }
    }
}
