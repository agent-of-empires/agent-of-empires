//! Right-click on a sidebar row opens a small popup menu (Rename /
//! Delete) anchored to the click. Picking Rename routes through the
//! same helper as the `r` key, Delete through the same helper as
//! `d`. Click-outside dismisses the menu.

use super::*;
use crate::session::config::SortOrder;
use crate::session::Item;
use crate::tui::dialogs::ContextMenuAction;
use ratatui::layout::Rect;

fn setup_inner(env: &mut TestEnv) {
    env.view.list_inner_area = Rect::new(1, 1, 28, 10);
    env.view.list_area = Rect::new(0, 0, 30, 12);
}

#[test]
#[serial]
fn right_click_on_session_opens_session_menu_and_moves_cursor() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    env.view.cursor = 0;
    env.view.update_selected();

    // Click the third visible row (inner.y + 2 == 3) -> flat_items[2].
    assert!(env.view.handle_right_click(5, 3));
    assert_eq!(env.view.cursor, 2, "cursor should move to clicked row");
    let menu = env
        .view
        .context_menu
        .as_ref()
        .expect("context_menu should be open");
    assert_eq!(menu.selected_action(), ContextMenuAction::NewFromSelection);
    // The selected item is a session, not a group.
    assert!(matches!(
        env.view.flat_items[env.view.cursor],
        Item::Session { .. }
    ));
}

#[test]
#[serial]
fn right_click_off_list_is_noop() {
    let mut env = create_test_env_with_sessions(3);
    setup_inner(&mut env);
    // Row 50 is well past list_inner_area.bottom.
    assert!(!env.view.handle_right_click(5, 50));
    assert!(env.view.context_menu.is_none());
}

#[test]
#[serial]
fn right_click_on_group_uses_group_menu() {
    let mut env = create_test_env_with_groups();
    setup_inner(&mut env);
    // Find a group row index in flat_items.
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("manual-mode test env should have a group row");
    let click_row = env.view.list_inner_area.y + group_idx as u16;

    assert!(env.view.handle_right_click(5, click_row));
    assert_eq!(env.view.cursor, group_idx);
    assert!(env.view.context_menu.is_some());
    assert!(matches!(
        env.view.flat_items[env.view.cursor],
        Item::Group { .. }
    ));
}

#[test]
#[serial]
fn enter_rename_in_menu_opens_rename_dialog() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 1);
    assert!(env.view.context_menu.is_some());
    // First item is New Session; Rename is one Down away. Enter submits it.
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(
        env.view.context_menu.is_none(),
        "menu should close on submit"
    );
    assert!(
        env.view.rename_dialog.is_some(),
        "Rename should route to rename_dialog like the 'r' key"
    );
}

#[test]
#[serial]
fn down_then_enter_in_menu_opens_delete_dialog() {
    let mut env = create_test_env_with_sessions(2);
    disable_delete_to_trash();
    setup_inner(&mut env);
    // Attention sort surfaces the full session menu (New Session / Rename
    // / Archive / Snooze / Mark unread / Add project / Delete), so Delete is
    // six Downs away. (Unread defaults on, so the "Mark unread" row is
    // present.)
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    env.view.handle_right_click(5, 1);
    for _ in 0..6 {
        env.view.handle_key(key(KeyCode::Down), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.context_menu.is_none());
    assert!(
        env.view.unified_delete_dialog.is_some(),
        "Delete should route to unified_delete_dialog like the 'd' key"
    );
}

#[test]
#[serial]
fn esc_in_menu_cancels_without_dialog() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 1);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.context_menu.is_none());
    assert!(env.view.rename_dialog.is_none());
    assert!(env.view.unified_delete_dialog.is_none());
}

/// Right-click a session, pick the Archive item (New Session -> Rename ->
/// Archive is two Downs), and the row gets archived through the same `z`
/// codepath. No follow-up dialog: archiving is immediate.
#[test]
#[serial]
fn right_click_archive_action_archives_session() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 1);
    let id = env.view.selected_session.clone().unwrap();
    assert!(
        !env.view.get_instance(&id).unwrap().is_archived(),
        "precondition: session starts unarchived"
    );

    env.view.handle_key(key(KeyCode::Down), None); // New Session -> Rename
    env.view.handle_key(key(KeyCode::Down), None); // Rename -> Archive
    env.view.handle_key(key(KeyCode::Enter), None);

    assert!(env.view.context_menu.is_none(), "menu closes after archive");
    assert!(
        env.view.get_instance(&id).unwrap().is_archived(),
        "context-menu Archive must archive the session"
    );
}

/// An archived row's context menu offers Unarchive, and picking it restores
/// the session.
#[test]
#[serial]
fn right_click_unarchive_action_restores_session() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    // Reveal the section and archive the first row so it stays visible.
    env.view.archived_section_collapsed = false;
    env.view.cursor = 0;
    env.view.update_selected();
    let id = env.view.selected_session.clone().unwrap();
    env.view.toggle_archive_at_cursor().unwrap();
    assert!(env.view.get_instance(&id).unwrap().is_archived());

    // Right-click the archived row: its menu must read "Unarchive".
    let idx = env
        .view
        .flat_items
        .iter()
        .position(|it| matches!(it, Item::Session { id: i, .. } if i == &id))
        .expect("archived row must be visible");
    // The archived session row renders in the pinned shelf; render a real
    // frame so the shelf rect is populated, then right-click that row.
    render_geometry(&mut env.view);
    let row = shelf_row_for_idx(&env.view, idx);
    assert!(env.view.handle_right_click(5, row));
    let labels: Vec<&str> = env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .map(|(_, l)| *l)
        .collect();
    // Default sort here is Newest, where Snooze is gated out. The unread
    // toggle is always-on (any sort) and defaults on. The default session
    // tool is claude (a forkable terminal agent), so the Fork row shows;
    // `right_click_session_menu_hides_fork_for_unforkable_agent` covers the
    // gated-off case. Menu is New Session / Rename / Unarchive / Mark unread
    // / Add project / Delete / Fork.
    assert_eq!(
        labels,
        vec![
            "New Session",
            "Rename",
            "Unarchive",
            "Mark unread",
            "Add project",
            "Delete",
            "Fork session"
        ]
    );

    env.view.handle_key(key(KeyCode::Down), None); // New Session -> Rename
    env.view.handle_key(key(KeyCode::Down), None); // Rename -> Unarchive
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(
        !env.view.get_instance(&id).unwrap().is_archived(),
        "context-menu Unarchive must unarchive the session"
    );
}

/// A forkable agent (claude, the default test tool) shows the "Fork
/// session" row so the mouse path matches the palette action.
#[test]
#[serial]
fn right_click_session_menu_shows_fork_for_forkable_agent() {
    let mut env = create_test_env_with_sessions(1);
    setup_inner(&mut env);
    assert!(env.view.handle_right_click(5, 1));
    let actions: Vec<ContextMenuAction> = env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .map(|(a, _)| *a)
        .collect();
    assert!(
        actions.contains(&ContextMenuAction::Fork),
        "a forkable agent (claude) must show the Fork row"
    );
}

/// A resume-only agent (gemini declares `ForkStrategy::Unsupported`) cannot
/// fork, so the menu must omit the "Fork session" row rather than offer an
/// action the palette would refuse.
#[test]
#[serial]
fn right_click_session_menu_hides_fork_for_unforkable_agent() {
    let mut env = create_test_env_with_sessions(1);
    setup_inner(&mut env);
    let id = match &env.view.flat_items[0] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("expected a session row"),
    };
    env.view
        .apply_user_action(&id, |inst| inst.tool = "gemini".to_string())
        .unwrap();
    env.view.flat_items = env.view.build_flat_items();
    assert!(env.view.handle_right_click(5, 1));
    let actions: Vec<ContextMenuAction> = env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .map(|(a, _)| *a)
        .collect();
    assert!(
        !actions.contains(&ContextMenuAction::Fork),
        "a resume-only agent (gemini) must not show the Fork row"
    );
}

/// The Snooze row mirrors the `'h'` keybinding, which only fires in
/// Attention sort. So the right-click session menu must omit Snooze in
/// every other sort and include it in Attention sort.
#[test]
#[serial]
fn right_click_session_menu_gates_snooze_to_attention_sort() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);

    let menu_actions = |env: &TestEnv| -> Vec<ContextMenuAction> {
        env.view
            .context_menu
            .as_ref()
            .unwrap()
            .items_for_test()
            .iter()
            .map(|(a, _)| *a)
            .collect()
    };

    // Newest sort (the default): no Snooze row.
    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    assert!(env.view.handle_right_click(5, 1));
    assert!(
        !menu_actions(&env).contains(&ContextMenuAction::ToggleSnooze),
        "Snooze must be hidden outside Attention sort"
    );
    env.view.context_menu = None;

    // Attention sort: Snooze row present.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    assert!(env.view.handle_right_click(5, 1));
    assert!(
        menu_actions(&env).contains(&ContextMenuAction::ToggleSnooze),
        "Snooze must appear in Attention sort"
    );
}

/// For a forkable agent the Fork row is sort-independent: unlike Snooze
/// (gated to Attention sort) it appears in every sort. Whether the row shows
/// at all is gated on fork capability, covered by the
/// `..._shows_fork_for_forkable_agent` / `..._hides_fork_for_unforkable_agent`
/// pair; this test pins that the capability gate does not accidentally
/// couple to sort order. The default test tool is claude (forkable).
#[test]
#[serial]
fn right_click_session_menu_offers_fork_in_every_sort_for_forkable_agent() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);

    let has_fork = |env: &TestEnv| -> bool {
        env.view
            .context_menu
            .as_ref()
            .unwrap()
            .items_for_test()
            .iter()
            .any(|(a, _)| *a == ContextMenuAction::Fork)
    };

    for sort in [SortOrder::Newest, SortOrder::Attention] {
        env.view.sort_order = sort;
        env.view.flat_items = env.view.build_flat_items();
        assert!(env.view.handle_right_click(5, 1));
        assert!(
            has_fork(&env),
            "Fork must be offered for a forkable agent in {sort:?} sort"
        );
        env.view.context_menu = None;
    }
}

#[test]
#[serial]
fn right_click_is_gated_when_other_dialog_is_open() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.show_help = true;
    assert!(env.view.has_dialog());
    // resolve_row_to_index short-circuits on any non-live-send overlay,
    // so the right-click handler should bail without opening the menu.
    assert!(!env.view.handle_right_click(5, 1));
    assert!(env.view.context_menu.is_none());
}

#[test]
#[serial]
fn context_menu_counts_as_dialog() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    assert!(!env.view.has_dialog());
    env.view.handle_right_click(5, 1);
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn left_click_outside_menu_dismisses_it() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 1);
    assert!(env.view.context_menu.is_some());
    // Before a render captures the menu's last_area, every click
    // reads as "outside", which is exactly the dismissal contract
    // we want here. (Item-row hit testing has its own unit coverage
    // in `dialogs::context_menu`.)
    let consumed = env.view.handle_context_menu_click(99, 99);
    assert!(consumed, "router should mark the click consumed");
    assert!(
        env.view.context_menu.is_none(),
        "outside click should dismiss the menu"
    );
}

#[test]
#[serial]
fn handle_context_menu_click_returns_false_when_no_menu() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    assert!(env.view.context_menu.is_none());
    assert!(!env.view.handle_context_menu_click(5, 5));
}

#[test]
#[serial]
fn left_click_on_empty_sidebar_outside_live_mode_is_noop() {
    // Left-click on empty sidebar space is intentionally low-stakes:
    // it does NOT open the new-session dialog anymore (right-click
    // owns that entry point) and it doesn't move selection. The
    // user can keep clicking the empty area to dismiss preview
    // selections without summoning modals.
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    // Sessions occupy inner rows 0 and 1 (y=1, y=2). Row 5 is well
    // past the last item but still inside list_inner_area.
    assert!(!env.view.handle_empty_list_click(5, 5));
    assert!(env.view.new_dialog.is_none());
    assert!(env.view.context_menu.is_none());
}

#[test]
#[serial]
fn left_click_on_empty_sidebar_in_live_mode_exits_live_mode() {
    // Quick-exit gesture: when live-send is active, a click on the
    // empty sidebar drops the user out of live mode. Mirrors the
    // Ctrl+Q chord but with the mouse, so a user who came in via
    // a left-click can also leave that way.
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    use crate::tui::home::live_send;
    env.view.live_send = Some(live_send::LiveSendState {
        session_id: "fake".to_string(),
        title: "fake".to_string(),
        tmux_name: "aoe_test_empty_click_exit_live".to_string(),
        target: live_send::LiveSendTarget::Agent,
        exit_chords: live_send::parse_chord_list(live_send::DEFAULT_EXIT_CHORD),
        leader: None,
    });
    assert!(env.view.live_send.is_some());
    assert!(env.view.handle_empty_list_click(5, 5));
    assert!(
        env.view.live_send.is_none(),
        "click on empty sidebar should exit live mode"
    );
    assert!(env.view.new_dialog.is_none());
}

#[test]
#[serial]
fn click_on_a_real_row_does_not_change_empty_click_state() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    // Row 1 resolves to flat_items[0], a real session row. The
    // empty-list click handler must defer to the regular click
    // path; it shouldn't open new-session or exit live mode here.
    assert!(!env.view.handle_empty_list_click(5, 1));
    assert!(env.view.new_dialog.is_none());
}

#[test]
#[serial]
fn empty_sidebar_click_is_gated_when_overlay_is_open() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.show_help = true;
    assert!(!env.view.handle_empty_list_click(5, 5));
    assert!(env.view.new_dialog.is_none());
}

#[test]
#[serial]
fn right_click_on_empty_sidebar_opens_empty_menu() {
    // Right-clicking the empty area of the sidebar (below the last
    // session) opens the dedicated 3-item menu so the mouse can
    // reach New / Sort / Grouping the same way `n`/`o`/`g` would
    // from the keyboard.
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    assert!(env.view.handle_right_click(5, 5));
    let menu = env.view.context_menu.as_ref().expect("menu opened");
    let labels: Vec<String> = menu
        .items_for_test()
        .iter()
        .map(|(_, label)| (*label).to_string())
        .collect();
    assert_eq!(
        labels,
        vec!["New Session", "Change Sort", "Change Grouping"]
    );
}

/// Helper: hit a key through the home view's handle_key path so
/// the dispatch tests run the same wiring real input does. Both
/// click and keyboard funnel through `dispatch_context_menu_action`,
/// so this covers the dispatcher without having to mock the menu's
/// `last_area` for hit-testing.
fn send_key(env: &mut crate::tui::home::tests::TestEnv, code: crossterm::event::KeyCode) {
    env.view.handle_key(
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        None,
    );
}

#[test]
#[serial]
fn empty_sidebar_menu_new_session_dispatches() {
    // First item (New Session) submits through the shared
    // dispatcher and opens the new-session dialog.
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 5);
    send_key(&mut env, crossterm::event::KeyCode::Enter);
    assert!(env.view.context_menu.is_none());
    assert!(env.view.new_dialog.is_some());
}

#[test]
#[serial]
fn empty_sidebar_menu_sort_dispatches() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 5);
    send_key(&mut env, crossterm::event::KeyCode::Down); // highlight "Change Sort"
    send_key(&mut env, crossterm::event::KeyCode::Enter);
    assert!(env.view.context_menu.is_none());
    assert!(env.view.sort_picker_dialog.is_some());
}

#[test]
#[serial]
fn empty_sidebar_menu_grouping_dispatches() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 5);
    send_key(&mut env, crossterm::event::KeyCode::Down);
    send_key(&mut env, crossterm::event::KeyCode::Down); // highlight "Change Grouping"
    send_key(&mut env, crossterm::event::KeyCode::Enter);
    assert!(env.view.context_menu.is_none());
    assert!(env.view.group_picker_dialog.is_some());
}

#[test]
#[serial]
fn empty_sidebar_menu_n_hotkey_opens_new_session() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 5);
    send_key(&mut env, crossterm::event::KeyCode::Char('n'));
    assert!(env.view.context_menu.is_none());
    assert!(env.view.new_dialog.is_some());
}

#[test]
#[serial]
fn empty_sidebar_menu_o_hotkey_opens_sort_picker() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 5);
    send_key(&mut env, crossterm::event::KeyCode::Char('o'));
    assert!(env.view.context_menu.is_none());
    assert!(env.view.sort_picker_dialog.is_some());
}

#[test]
#[serial]
fn empty_sidebar_menu_g_hotkey_opens_group_picker() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 5);
    send_key(&mut env, crossterm::event::KeyCode::Char('g'));
    assert!(env.view.context_menu.is_none());
    assert!(env.view.group_picker_dialog.is_some());
}

#[test]
#[serial]
fn session_menu_n_hotkey_opens_new_session() {
    // The session-row menu now carries a New Session entry (issue #2023),
    // so 'n' submits NewFromSelection just like the group/project menus,
    // closing the menu and opening the new-session dialog prefilled from
    // the right-clicked session.
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 1); // row 1 = first session
    send_key(&mut env, crossterm::event::KeyCode::Char('n'));
    assert!(
        env.view.context_menu.is_none(),
        "menu should close on submit"
    );
    assert!(
        env.view.new_dialog.is_some(),
        "n on session menu must open the new-session dialog"
    );
}
