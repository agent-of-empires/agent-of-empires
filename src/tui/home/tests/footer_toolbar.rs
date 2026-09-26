//! The footer hint bar is a clickable toolbar: each shortcut renders a
//! hit rect paired with the key it synthesizes, a click dispatches that
//! key through the normal handler, and hover highlights the button.
use super::*;
use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::Terminal;

fn render_at(env: &mut TestEnv, w: u16, h: u16) {
    let theme = crate::tui::styles::load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
}

fn button_key(env: &TestEnv, code: KeyCode) -> Option<KeyEvent> {
    env.view
        .footer_buttons
        .iter()
        .find(|(_, k)| k.code == code)
        .map(|(_, k)| *k)
}

/// Each rendered shortcut produces a hit rect carrying the equivalent key: a click inside
/// one resolves to that key and dispatching it runs the keypress's action, hover tracks the
/// button under the pointer, and an open non-live overlay (help) blocks footer and sidebar
/// clicks behind it.
#[test]
#[serial]
fn buttons_map_clicks_hover_and_yield_to_overlays() {
    let mut env = create_test_env_with_sessions(3);
    render_at(&mut env, 120, 12);

    // New / View / Group / Cmds are always present in the home view.
    assert_eq!(
        button_key(&env, KeyCode::Char('n')).map(|k| k.modifiers),
        Some(KeyModifiers::NONE),
        "New maps to a plain 'n'"
    );
    let cmds = button_key(&env, KeyCode::Char('k')).expect("Cmds button present");
    assert_eq!(cmds.modifiers, KeyModifiers::CONTROL, "Cmds maps to Ctrl+K");

    let (new_rect, new_key) = env
        .view
        .footer_buttons
        .iter()
        .find(|(_, k)| k.code == KeyCode::Char('n'))
        .cloned()
        .expect("New button rect");
    assert_eq!(
        env.view
            .footer_button_at(new_rect.x, new_rect.y)
            .map(|k| k.code),
        Some(KeyCode::Char('n'))
    );
    assert!(
        env.view
            .footer_button_at(new_rect.x, new_rect.y + 5)
            .is_none(),
        "a click off the footer row hits no button"
    );

    let (rect, hover_key) = env.view.footer_buttons[1];
    assert!(env.view.footer_hover.is_none());
    assert!(
        env.view.handle_hover(rect.x + 1, rect.y),
        "moving onto a button is a hover change"
    );
    assert_eq!(env.view.footer_hover, Some(hover_key));
    assert!(
        !env.view.handle_hover(rect.x, rect.y),
        "same button, no change"
    );
    assert!(env.view.handle_hover(rect.x, rect.y.saturating_sub(5)));
    assert!(
        env.view.footer_hover.is_none(),
        "leaving the footer clears hover"
    );

    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(new_key, None);
    assert!(
        env.view.new_dialog.is_some(),
        "clicking New opens the new-session dialog"
    );
    env.view.new_dialog = None;

    env.view.show_help = true;
    assert!(
        env.view.has_non_live_send_overlay(),
        "help screen is a non-live overlay"
    );
    assert!(
        env.view.footer_button_at(new_rect.x, new_rect.y).is_none(),
        "footer click is blocked while an overlay owns the screen"
    );
    assert!(
        !env.view.handle_sidebar_collapse_click(0, 0),
        "sidebar toggle is blocked while an overlay owns the screen"
    );
}

/// Strict-hotkey mode shifts the chords: Diff becomes Ctrl+D and Delete
/// becomes an uppercase 'D', and the buttons synthesize those exactly.
#[test]
#[serial]
fn strict_mode_buttons_carry_shifted_chords() {
    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = true;
    render_at(&mut env, 120, 12);

    let diff = button_key(&env, KeyCode::Char('d')).expect("Diff button (strict ^D)");
    assert_eq!(
        diff.modifiers,
        KeyModifiers::CONTROL,
        "strict Diff is Ctrl+D"
    );
    assert!(
        button_key(&env, KeyCode::Char('D')).is_some(),
        "strict Delete is an uppercase D"
    );
}

/// The footer is replaced by the live-send banner, so it exposes no
/// clickable buttons while live mode owns the status bar.
#[test]
#[serial]
fn no_buttons_during_live_send() {
    let mut env = create_test_env_with_sessions(3);
    render_at(&mut env, 120, 12);
    assert!(!env.view.footer_buttons.is_empty());

    env.view.cursor = 1;
    env.view.update_selected();
    let id = match env.view.flat_items.get(1) {
        Some(Item::Session { id, .. }) => id.clone(),
        _ => panic!("expected a session at flat_items[1]"),
    };
    env.view.live_send = Some(crate::tui::home::live_send::LiveSendState {
        session_id: id,
        title: "s".to_string(),
        tmux_name: "fake".to_string(),
        target: crate::tui::home::live_send::LiveSendTarget::Agent,
        exit_chords: crate::tui::home::live_send::parse_chord_list(
            crate::tui::home::live_send::DEFAULT_EXIT_CHORD,
        ),
        leader: None,
    });
    render_at(&mut env, 120, 12);
    assert!(
        env.view.footer_buttons.is_empty(),
        "live-send banner replaces the footer toolbar"
    );
    assert_eq!(env.view.footer_button_at(0, 11), None);
}
