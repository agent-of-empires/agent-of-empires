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

/// Each rendered shortcut produces a hit rect carrying the equivalent
/// key, and `footer_button_at` resolves a click inside one to that key.
#[test]
#[serial]
fn buttons_map_clicks_to_shortcuts() {
    let mut env = create_test_env_with_sessions(3);
    render_at(&mut env, 120, 12);

    assert!(
        !env.view.footer_buttons.is_empty(),
        "footer should expose clickable buttons"
    );
    // New / View / Group / Cmds are always present in the home view.
    assert_eq!(
        button_key(&env, KeyCode::Char('n')).map(|k| k.modifiers),
        Some(KeyModifiers::NONE),
        "New maps to a plain 'n'"
    );
    let cmds = button_key(&env, KeyCode::Char('k')).expect("Cmds button present");
    assert_eq!(cmds.modifiers, KeyModifiers::CONTROL, "Cmds maps to Ctrl+K");

    // A click inside the New button's rect resolves to its key; a click
    // outside every button resolves to nothing.
    let (new_rect, _) = env
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
}

/// Dispatching a footer button's key runs the same action as the
/// keypress: the New button opens the new-session dialog.
#[test]
#[serial]
fn clicking_new_opens_dialog() {
    let mut env = create_test_env_with_sessions(3);
    render_at(&mut env, 120, 12);
    let key = button_key(&env, KeyCode::Char('n')).expect("New button");
    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key, None);
    assert!(
        env.view.new_dialog.is_some(),
        "clicking New opens the new-session dialog"
    );
}

/// While a non-live overlay (here the help screen) is open, the footer
/// is drawn underneath but the overlay owns clicks: `footer_button_at`
/// and `handle_sidebar_collapse_click` must report no hit so a click
/// can't fire a shortcut or toggle the sidebar behind the modal.
#[test]
#[serial]
fn overlay_blocks_footer_and_sidebar_clicks() {
    let mut env = create_test_env_with_sessions(3);
    render_at(&mut env, 120, 12);
    let (rect, _) = env.view.footer_buttons[0];
    // No overlay: the button resolves.
    assert!(env.view.footer_button_at(rect.x, rect.y).is_some());

    env.view.show_help = true;
    assert!(
        env.view.has_non_live_send_overlay(),
        "help screen is a non-live overlay"
    );
    assert!(
        env.view.footer_button_at(rect.x, rect.y).is_none(),
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

/// Hover sets `footer_hover` to the button under the pointer, reports the
/// change, and clears when the pointer leaves the footer.
#[test]
#[serial]
fn hover_tracks_button_under_pointer() {
    let mut env = create_test_env_with_sessions(3);
    render_at(&mut env, 120, 12);
    let (rect, key) = env.view.footer_buttons[1];

    assert!(env.view.footer_hover.is_none());
    let changed = env.view.handle_hover(rect.x + 1, rect.y);
    assert!(changed, "moving onto a button is a hover change");
    assert_eq!(env.view.footer_hover, Some(key));

    // Same button, no change reported.
    assert!(!env.view.handle_hover(rect.x, rect.y));

    // Off the footer row clears the hover.
    let changed = env.view.handle_hover(rect.x, rect.y.saturating_sub(5));
    assert!(changed);
    assert!(env.view.footer_hover.is_none());
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
