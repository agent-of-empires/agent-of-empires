//! Right-click on a sidebar row opens a popup menu anchored to the click. Rename routes
//! through the same helper as the `r` key, Delete through the same helper as `d`, and a
//! click outside dismisses the menu.

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
    assert!(!env.view.has_dialog());

    // Click the third visible row (inner.y + 2 == 3) -> flat_items[2].
    assert!(env.view.handle_right_click(5, 3));
    assert_eq!(env.view.cursor, 2, "cursor should move to clicked row");
    assert!(
        env.view.has_dialog(),
        "an open context menu counts as a dialog"
    );
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

/// Session-menu entries route through the same helpers as their keys: Rename like `r`,
/// Archive like `z` (immediate, no dialog), Delete like `d`. Esc cancels without a dialog.
/// Attention sort shows the full menu: New Session / Rename / Archive / Snooze / Mark unread
/// / Add project / Delete.
#[test]
#[serial]
fn session_menu_entries_route_like_their_keys() {
    type Check = fn(&HomeView, &str) -> bool;
    let cases: [(&str, usize, KeyCode, Check); 4] = [
        ("rename", 1, KeyCode::Enter, |v, _| {
            v.rename_dialog.is_some()
        }),
        ("archive", 2, KeyCode::Enter, |v, id| {
            v.get_instance(id).unwrap().is_archived()
        }),
        ("delete", 6, KeyCode::Enter, |v, _| {
            v.unified_delete_dialog.is_some()
        }),
        ("esc", 0, KeyCode::Esc, |v, _| {
            v.rename_dialog.is_none() && v.unified_delete_dialog.is_none()
        }),
    ];
    for (label, downs, submit, check) in cases {
        let mut env = create_test_env_with_sessions(2);
        disable_delete_to_trash();
        setup_inner(&mut env);
        env.view.sort_order = SortOrder::Attention;
        env.view.flat_items = env.view.build_flat_items();
        assert!(env.view.handle_right_click(5, 1), "{label}");
        let id = env.view.selected_session.clone().unwrap();
        assert!(
            !env.view.get_instance(&id).unwrap().is_archived(),
            "{label}"
        );
        for _ in 0..downs {
            env.view.handle_key(key(KeyCode::Down), None);
        }
        env.view.handle_key(key(submit), None);
        assert!(env.view.context_menu.is_none(), "{label}: menu closes");
        assert!(check(&env.view, &id), "{label}");
    }
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

    env.view.handle_key(key(KeyCode::Down), None); // New Session -> Rename
    env.view.handle_key(key(KeyCode::Down), None); // Rename -> Unarchive
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(
        !env.view.get_instance(&id).unwrap().is_archived(),
        "context-menu Unarchive must unarchive the session"
    );
}

#[test]
#[serial]
fn right_click_fork_requires_provenance_not_a_tool_label() {
    let mut env = create_test_env_empty();
    let mut parent = observed_fork_parent("claude");
    let id = parent.id.clone();
    let binding = parent.agent_session_binding.take();
    env.view.add_instance(parent);
    env.view.flat_items = env.view.build_flat_items();
    setup_inner(&mut env);
    assert!(env.view.handle_right_click(5, 1));
    assert!(!env
        .view
        .context_menu
        .as_ref()
        .unwrap()
        .items_for_test()
        .iter()
        .any(|(action, _)| *action == ContextMenuAction::Fork));
    env.view.context_menu = None;
    env.view
        .apply_user_action(&id, |instance| {
            instance.agent_session_binding = binding;
            instance.tool = "status-alias".into();
        })
        .unwrap();
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

/// A resume-only agent (gemini declares `ForkStrategy::Unsupported`) cannot fork, so the
/// menu omits the "Fork session" row rather than offering an action the palette would
/// refuse.
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

/// The Snooze row mirrors the `'h'` keybinding, which fires only in Attention sort, so the
/// menu omits it in every other sort. For a forkable agent the Fork row is sort-independent.
#[test]
#[serial]
fn right_click_session_menu_gates_snooze_to_attention_sort() {
    let mut env = create_test_env_empty();
    env.view.add_instance(observed_fork_parent("claude"));
    env.view.add_instance(observed_fork_parent("claude"));
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
    assert!(menu_actions(&env).contains(&ContextMenuAction::Fork));
    env.view.context_menu = None;

    // Attention sort: Snooze row present.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    assert!(env.view.handle_right_click(5, 1));
    assert!(
        menu_actions(&env).contains(&ContextMenuAction::ToggleSnooze),
        "Snooze must appear in Attention sort"
    );
    assert!(menu_actions(&env).contains(&ContextMenuAction::Fork));
}

#[test]
#[serial]
fn left_click_outside_menu_dismisses_it() {
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    env.view.handle_right_click(5, 1);
    assert!(env.view.context_menu.is_some());
    // Before a render captures the menu's last_area every click reads as "outside", which
    // is the dismissal contract here; item-row hit testing is covered in
    // `dialogs::context_menu`.
    let consumed = env.view.handle_context_menu_click(99, 99);
    assert!(consumed, "router should mark the click consumed");
    assert!(
        env.view.context_menu.is_none(),
        "outside click should dismiss the menu"
    );
}

/// Clicks that open neither a menu nor a dialog. Left-click on empty sidebar space is
/// deliberately low-stakes outside live mode (right-click owns New Session), a real row
/// defers to the regular click path, and any non-live overlay gates both handlers.
#[test]
#[serial]
fn sidebar_clicks_that_open_nothing() {
    type Click = fn(&mut HomeView) -> bool;
    // Sessions occupy inner rows 0 and 1 (y=1, y=2); y=5 is empty list space, y=50 is
    // past the list.
    let cases: [(&str, bool, Click); 6] = [
        ("right-click past the list", false, |v| {
            v.handle_right_click(5, 50)
        }),
        ("right-click under an overlay", true, |v| {
            v.handle_right_click(5, 1)
        }),
        ("empty-space click", false, |v| {
            v.handle_empty_list_click(5, 5)
        }),
        ("empty-list click on a real row", false, |v| {
            v.handle_empty_list_click(5, 1)
        }),
        ("empty-space click under an overlay", true, |v| {
            v.handle_empty_list_click(5, 5)
        }),
        ("menu click with no menu open", false, |v| {
            v.handle_context_menu_click(5, 5)
        }),
    ];
    let mut env = create_test_env_with_sessions(2);
    setup_inner(&mut env);
    for (label, overlay, click) in cases {
        env.view.show_help = overlay;
        assert!(!click(&mut env.view), "{label}");
        assert!(env.view.context_menu.is_none(), "{label}");
        assert!(env.view.new_dialog.is_none(), "{label}");
    }
}

#[test]
#[serial]
fn left_click_on_empty_sidebar_in_live_mode_exits_live_mode() {
    // Quick-exit gesture: with live-send active, a click on the empty sidebar drops out of
    // live mode, mirroring Ctrl+Q for users who came in by clicking.
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
fn right_click_on_empty_sidebar_opens_empty_menu() {
    // Right-clicking the empty area opens the dedicated 3-item menu, so the mouse reaches
    // New / Sort / Grouping the way `n`/`o`/`g` do.
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

/// Hit a key through the home view's handle_key path so the dispatch tests run the wiring
/// real input does. Click and keyboard both funnel through `dispatch_context_menu_action`,
/// so this covers the dispatcher without mocking the menu's `last_area`.
fn send_key(env: &mut crate::tui::home::tests::TestEnv, code: crossterm::event::KeyCode) {
    env.view.handle_key(
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
        None,
    );
}

/// Each empty-sidebar entry submits through the shared dispatcher and opens its dialog.
#[test]
#[serial]
fn empty_sidebar_menu_entries_dispatch() {
    type Opened = fn(&HomeView) -> bool;
    let cases: [(&str, usize, Opened); 3] = [
        ("New Session", 0, |v| v.new_dialog.is_some()),
        ("Change Sort", 1, |v| v.sort_picker_dialog.is_some()),
        ("Change Grouping", 2, |v| v.group_picker_dialog.is_some()),
    ];
    for (label, downs, opened) in cases {
        let mut env = create_test_env_with_sessions(2);
        setup_inner(&mut env);
        env.view.handle_right_click(5, 5);
        for _ in 0..downs {
            send_key(&mut env, crossterm::event::KeyCode::Down);
        }
        send_key(&mut env, crossterm::event::KeyCode::Enter);
        assert!(env.view.context_menu.is_none(), "{label}");
        assert!(opened(&env.view), "{label}");
    }
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
    // The session-row menu carries a New Session entry (#2023), so 'n' submits
    // NewFromSelection like the group menus, closing the menu and opening the dialog
    // prefilled from the right-clicked session.
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
