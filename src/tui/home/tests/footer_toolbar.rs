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
        .find(|(k, _)| k.code == code)
        .map(|(k, _)| *k)
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

    let (new_key, new_rect) = env
        .view
        .footer_buttons
        .iter()
        .find(|(k, _)| k.code == KeyCode::Char('n'))
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

    let (hover_key, rect) = env.view.footer_buttons[1];
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
/// Rendered wide enough that every hint fits beside the tips badge.
#[test]
#[serial]
fn strict_mode_buttons_carry_shifted_chords() {
    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = true;
    render_at(&mut env, 160, 12);

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

/// Live mode replaces footer buttons with the configured chord hints.
#[test]
#[serial]
fn live_send_banner_names_space_chords_and_hides_buttons() {
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
        exit_chords: crate::tui::home::live_send::parse_chord_list("M-Space,C-q"),
        leader: crate::tui::home::live_send::parse_chord("C-Space"),
    });
    let screen = render_home_to_string(&mut env.view, 120, 12);
    assert!(screen.contains("Alt+Space / Ctrl+Q to exit"), "{screen}");
    assert!(screen.contains("Ctrl+Space menu"), "{screen}");
    assert!(
        env.view.footer_buttons.is_empty(),
        "live-send banner replaces the footer toolbar"
    );
    assert_eq!(env.view.footer_button_at(0, 11), None);

    env.view.live_send_pending_leader = true;
    let screen = render_home_to_string(&mut env.view, 120, 12);
    assert!(screen.contains("Ctrl+Space:"), "{screen}");
}

/// The running server status outlasts the other priority-0 hints: at 48 columns with tips
/// and a committed search, the search cue fills the space `?` would need, not the status.
#[test]
#[serial]
fn running_web_status_survives_a_committed_search_on_a_narrow_footer() {
    let mut env = create_test_env_with_sessions(3);
    env.view.tips_unseen = 3;
    for c in ['/', 's', 'e', 's', 's'] {
        env.view.handle_key(key(KeyCode::Char(c)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    let shown = |env: &TestEnv, code| env.view.footer_buttons.iter().any(|(k, _)| k.code == code);
    render_at(&mut env, 48, 12);
    assert!(shown(&env, KeyCode::Char('n')), "search cue kept");
    assert!(!shown(&env, KeyCode::Char('R')), "idle hint drops first");
    env.view.serve_running = true;
    render_at(&mut env, 48, 12);
    assert!(shown(&env, KeyCode::Char('R')), "running status kept");
    assert!(shown(&env, KeyCode::Enter), "Enter kept");
}

/// The web dashboard entry and New survive the narrow footers the bar is tuned for: with a
/// tips badge showing, `R` (strict: Ctrl+R), New and Msg stay at 80 and 60 columns, and
/// Fav, now below them, is the one that drops.
#[test]
#[serial]
fn web_entry_and_new_survive_narrow_footers() {
    let mut env = create_test_env_with_sessions(1);
    let original = crate::session::favorites_first();
    crate::session::set_favorites_first(true);
    env.view.tips_unseen = 3;
    let plain = |c| (KeyCode::Char(c), KeyModifiers::NONE);
    for (strict, width, fav_kept) in [
        (false, 120, true),
        (false, 80, false),
        (false, 60, false),
        (true, 80, false),
    ] {
        env.view.strict_hotkeys = strict;
        render_at(&mut env, width, 12);
        let shown = |(code, modifiers): (KeyCode, KeyModifiers)| {
            env.view
                .footer_buttons
                .iter()
                .any(|(k, _)| k.code == code && k.modifiers == modifiers)
        };
        let (web, new, msg, fav) = if strict {
            (
                (KeyCode::Char('r'), KeyModifiers::CONTROL),
                plain('N'),
                plain('M'),
                plain('F'),
            )
        } else {
            (plain('R'), plain('n'), plain('m'), plain('f'))
        };
        let case = format!("strict={strict} width={width}");
        assert!(shown(web), "web entry kept: {case}");
        assert!(shown(new), "New kept: {case}");
        assert!(shown(msg), "Msg kept: {case}");
        assert_eq!(shown(fav), fav_kept, "Fav: {case}");
    }
    crate::session::set_favorites_first(original);
}
