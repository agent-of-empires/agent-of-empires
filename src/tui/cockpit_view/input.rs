//! Focus model + key dispatch for the cockpit view.
//!
//! Three focusable regions: composer, transcript, and (when one is
//! pending) approval card. The composer captures **every** key when
//! focused, including `a`/`A`/`d`, so typing "always allow" into a
//! prompt never resolves an approval. `Esc` from any region except
//! composer exits the view; from composer it returns focus to the
//! transcript.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cockpit::protocol::ApprovalDecisionWire;

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
    /// Cancel the in-flight prompt (Ctrl-C style).
    CancelInFlight,
    /// Drop every queued (not-yet-sent) prompt.
    ClearQueue,
    /// Open the daemon URL for this session in the user's browser.
    OpenInBrowser,
    /// Move focus to the named region.
    SetFocus(Focus),
    /// Exit the cockpit view; return to the home screen.
    Exit,
    /// Nothing to do (unhandled key).
    Ignore,
}

/// Translate a key event into an [`Intent`] based on the current
/// focus. Pure function so the entire focus model is unit-testable
/// without instantiating a real ratatui surface.
pub fn dispatch(focus: Focus, key: &KeyEvent, has_pending_approval: bool) -> Intent {
    // Universal: Ctrl-C cancels any in-flight prompt (matches the web
    // composer's stop button). We intentionally do NOT exit the view
    // on Ctrl-C because the user's natural reflex from a tmux session
    // is "stop the agent, don't quit the screen."
    if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
        return Intent::CancelInFlight;
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

    match focus {
        Focus::Composer => composer_keys(key),
        Focus::Transcript => transcript_keys(key, has_pending_approval),
        Focus::Approval => approval_keys(key),
    }
}

fn composer_keys(key: &KeyEvent) -> Intent {
    match (key.modifiers, key.code) {
        // Plain Enter submits.
        (m, KeyCode::Enter) if m.is_empty() => Intent::SubmitPrompt,
        // Shift+Enter inserts a newline (passed through to textarea).
        (m, KeyCode::Enter) if m.contains(KeyModifiers::SHIFT) => Intent::Compose(*key),
        // Esc moves focus to the transcript so the user can scroll or
        // pick an approval card. This also dismisses any accidental
        // composer focus (e.g. after typing then changing their mind).
        (m, KeyCode::Esc) if m.is_empty() => Intent::SetFocus(Focus::Transcript),
        // Tab cycles forward through the focus regions.
        (m, KeyCode::Tab) if m.is_empty() => Intent::SetFocus(Focus::Transcript),
        // Everything else is forwarded to the textarea, including
        // `a`/`A`/`d`. This is the focus-isolation guarantee.
        _ => Intent::Compose(*key),
    }
}

fn transcript_keys(key: &KeyEvent, has_pending_approval: bool) -> Intent {
    match (key.modifiers, key.code) {
        // Exit / dismiss.
        (m, KeyCode::Esc) if m.is_empty() => Intent::Exit,
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
        (m, KeyCode::PageDown) if m.is_empty() => Intent::Scroll(10),
        (m, KeyCode::PageUp) if m.is_empty() => Intent::Scroll(-10),
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
        (m, KeyCode::Esc) if m.is_empty() => Intent::SetFocus(Focus::Transcript),
        (m, KeyCode::Tab) if m.is_empty() => Intent::SetFocus(Focus::Composer),
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

    #[test]
    fn composer_swallows_approval_letters() {
        // Regression test for the composer-eats-approval bug: typing
        // "always allow" with a pending approval must NOT fire any
        // approval intent.
        for ch in "always allow deny".chars() {
            let intent = dispatch(Focus::Composer, &key(KeyCode::Char(ch)), true);
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
                true,
            );
            assert!(
                !matches!(intent, Intent::ResolveApproval(_)),
                "{ch} resolved from transcript focus: {intent:?}"
            );
        }
        // But the same letters DO resolve under approval focus.
        assert!(matches!(
            dispatch(Focus::Approval, &key(KeyCode::Char('a')), true),
            Intent::ResolveApproval(ApprovalDecisionWire::Allow)
        ));
        assert!(matches!(
            dispatch(
                Focus::Approval,
                &key_mod(KeyCode::Char('A'), KeyModifiers::SHIFT),
                true
            ),
            Intent::ResolveApproval(ApprovalDecisionWire::AllowAlways)
        ));
        assert!(matches!(
            dispatch(Focus::Approval, &key(KeyCode::Char('d')), true),
            Intent::ResolveApproval(ApprovalDecisionWire::Deny)
        ));
    }

    #[test]
    fn esc_from_composer_returns_focus_to_transcript() {
        let intent = dispatch(Focus::Composer, &key(KeyCode::Esc), false);
        assert_eq!(intent, Intent::SetFocus(Focus::Transcript));
    }

    #[test]
    fn esc_from_transcript_exits() {
        let intent = dispatch(Focus::Transcript, &key(KeyCode::Esc), false);
        assert_eq!(intent, Intent::Exit);
    }

    #[test]
    fn ctrl_c_cancels_from_any_focus() {
        for focus in [Focus::Composer, Focus::Transcript, Focus::Approval] {
            let intent = dispatch(
                focus,
                &key_mod(KeyCode::Char('c'), KeyModifiers::CONTROL),
                true,
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
                false,
            );
            assert_eq!(intent, Intent::ClearQueue);
        }
        // Plain 'x' in the composer is still a typed character.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('x')), false),
            Intent::Compose(_)
        ));
    }

    #[test]
    fn plain_o_opens_browser_only_from_transcript() {
        // Composer focus must pass through.
        let composer = dispatch(Focus::Composer, &key(KeyCode::Char('o')), false);
        assert!(matches!(composer, Intent::Compose(_)));
        // Transcript focus opens browser.
        let transcript = dispatch(Focus::Transcript, &key(KeyCode::Char('o')), false);
        assert_eq!(transcript, Intent::OpenInBrowser);
    }

    #[test]
    fn enter_in_composer_submits() {
        let intent = dispatch(Focus::Composer, &key(KeyCode::Enter), false);
        assert_eq!(intent, Intent::SubmitPrompt);
    }

    #[test]
    fn shift_enter_in_composer_inserts_newline() {
        let intent = dispatch(
            Focus::Composer,
            &key_mod(KeyCode::Enter, KeyModifiers::SHIFT),
            false,
        );
        assert!(matches!(intent, Intent::Compose(_)));
    }

    #[test]
    fn tab_from_transcript_routes_to_approval_when_pending() {
        let with_pending = dispatch(Focus::Transcript, &key(KeyCode::Tab), true);
        assert_eq!(with_pending, Intent::SetFocus(Focus::Approval));
        let without = dispatch(Focus::Transcript, &key(KeyCode::Tab), false);
        assert_eq!(without, Intent::SetFocus(Focus::Composer));
    }

    #[test]
    fn vim_scroll_keys_only_active_in_transcript() {
        assert_eq!(
            dispatch(Focus::Transcript, &key(KeyCode::Char('j')), false),
            Intent::Scroll(1)
        );
        // 'j' in composer is a typed character, not a scroll.
        assert!(matches!(
            dispatch(Focus::Composer, &key(KeyCode::Char('j')), false),
            Intent::Compose(_)
        ));
    }
}
