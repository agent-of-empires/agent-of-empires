//! Search mode: matching, committed queries, and cursor behavior.

use super::*;

#[test]
#[serial]
fn test_search_mode_esc_exits_and_clears() {
    let mut env = create_test_env_with_sessions(3);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('x')), None);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(!env.view.search_active);
    assert!(env.view.search_query.value().is_empty());
    assert!(env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_mode_enter_commits_without_clearing_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    assert!(env.view.search_active);
    let matches_before = env.view.search_matches.len();
    assert!(
        matches_before > 1,
        "test needs multiple matches to be meaningful"
    );

    env.view.handle_key(key(KeyCode::Enter), None);

    assert!(!env.view.search_active);
    assert_eq!(
        env.view.search_query.value(),
        "sess",
        "Enter must keep search_query so reloads re-score instead of wiping matches"
    );
    assert_eq!(
        env.view.search_matches.len(),
        matches_before,
        "Enter must not clear matches"
    );
    assert_eq!(env.view.search_match_index, 0);
}

#[test]
#[serial]
fn test_reload_after_enter_preserves_search_state() {
    // Regression guard for #2676: `refresh_search_matches` wipes matches
    // whenever the query is empty. If Enter cleared search_query, the very
    // next storage/config reload would destroy the matches Enter promised
    // to keep, silently breaking `n` match cycling.
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    let matches_before = env.view.search_matches.len();
    assert!(
        matches_before >= 3,
        "test needs matches for meaningful assertions"
    );

    env.view.reload().unwrap();

    assert_eq!(
        env.view.search_matches.len(),
        matches_before,
        "reload after Enter must not wipe search_matches"
    );

    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert_eq!(
        env.view.search_match_index, 1,
        "n still cycles after a reload lands between Enter and the first press"
    );
}

#[test]
#[serial]
fn test_sort_order_change_after_enter_rescores_search_matches() {
    // Regression guard for #2676: paths that rebuild `flat_items`
    // (`apply_sort_order`, `apply_group_by`, `toggle_group_collapsed`)
    // must re-score `search_matches` against the new indices, or `n`/`N`
    // jumps to stale positions and row highlights land on wrong sessions.
    // Query "session0" matches only session0. With Newest sort it sits at
    // index 4; with Oldest at index 0. Without the fix the stale index 4
    // would land on session4 after the sort change.
    use crate::session::config::SortOrder;
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    for c in "session0".chars() {
        env.view.handle_key(key(KeyCode::Char(c)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);

    assert_eq!(env.view.search_matches.len(), 1);
    let matched_id_before = match &env.view.flat_items[env.view.search_matches[0]] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("initial match must be a Session"),
    };

    let new_order = if env.view.sort_order == SortOrder::Newest {
        SortOrder::Oldest
    } else {
        SortOrder::Newest
    };
    env.view.apply_sort_order(new_order);

    assert_eq!(
        env.view.search_matches.len(),
        1,
        "same session still matches after sort"
    );
    let matched_id_after = match &env.view.flat_items[env.view.search_matches[0]] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("match must still be a Session, not a stale non-Session index"),
    };
    assert_eq!(
        matched_id_after, matched_id_before,
        "sort change must not orphan search_matches to a stale index pointing at the wrong session"
    );
}

#[test]
#[serial]
fn test_search_mode_enter_keeps_matches_for_cycling() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('e')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);

    let n_matches = env.view.search_matches.len();
    assert!(
        n_matches >= 3,
        "test needs at least 3 matches for wrap coverage"
    );
    assert_eq!(env.view.search_match_index, 0);

    for expected in 1..n_matches {
        env.view.handle_key(key(KeyCode::Char('n')), None);
        assert_eq!(env.view.search_match_index, expected);
    }

    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert_eq!(env.view.search_match_index, 0, "n wraps to first");

    // #3038: Shift+N never cycles. Even with a committed search live, it opens
    // the new-from-selection dialog and leaves the match index untouched.
    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert!(
        env.view.new_dialog.is_some(),
        "Shift+N opens new-from-selection even during a committed search"
    );
    assert_eq!(
        env.view.search_match_index, 0,
        "Shift+N must not cycle the search"
    );
}

#[test]
#[serial]
fn test_d_on_session_opens_delete_dialog() {
    let mut env = create_test_env_with_sessions(3);
    disable_delete_to_trash();
    env.view.update_selected();
    assert!(env.view.unified_delete_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(env.view.unified_delete_dialog.is_some());
}

#[test]
#[serial]
fn test_d_on_group_with_sessions_opens_group_delete_options_dialog() {
    let mut env = create_test_env_with_groups();
    env.view.cursor = 1;
    env.view.update_selected();
    assert!(env.view.selected_group.is_some());
    assert!(env.view.group_delete_options_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('d')), None);
    assert!(env.view.group_delete_options_dialog.is_some());
}

#[test]
#[serial]
fn test_selected_session_updates_on_cursor_move() {
    let mut env = create_test_env_with_sessions(3);
    let first_id = env.view.selected_session.clone();
    env.view.handle_key(key(KeyCode::Down), None);
    assert_ne!(env.view.selected_session, first_id);
}

#[test]
#[serial]
fn test_selected_group_set_when_on_group() {
    let mut env = create_test_env_with_groups();
    for i in 0..env.view.flat_items.len() {
        env.view.cursor = i;
        env.view.update_selected();
        if matches!(env.view.flat_items.get(i), Some(Item::Group { .. })) {
            assert!(env.view.selected_group.is_some());
            assert!(env.view.selected_session.is_none());
            return;
        }
    }
    panic!("No group found in flat_items");
}

#[test]
#[serial]
fn test_search_matches_session_title() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session2".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
    // The best match should be session2
    let best_idx = env.view.search_matches[0];
    if let Item::Session { id, .. } = &env.view.flat_items[best_idx] {
        let inst = env.view.get_instance(id).unwrap();
        assert!(inst.title.contains("session2"));
    }
}

#[test]
#[serial]
fn test_search_case_insensitive() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("SESSION2".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_matches_path() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("/tmp/3".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_matches_group_name() {
    let mut env = create_test_env_with_groups();
    env.view.search_query = Input::new("work".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_empty_query_clears_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    assert!(!env.view.search_matches.is_empty());

    env.view.search_query = Input::default();
    env.view.update_search();
    assert!(env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_no_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("zzzznonexistent".to_string());
    env.view.update_search();
    assert!(env.view.search_matches.is_empty());
}

#[test]
#[serial]
fn test_search_jumps_to_best_match() {
    let mut env = create_test_env_with_sessions(5);
    env.view.cursor = 0; // start at beginning
    env.view.search_active = true;
    env.view.search_query = Input::new("session0".to_string());
    env.view.update_search();
    // Cursor should jump to the best match
    // With default sort (Newest), session0 is at index 4 (last)
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_search_keeps_full_list() {
    let mut env = create_test_env_with_sessions(5);
    let original_len = env.view.flat_items.len();
    env.view.search_query = Input::new("session2".to_string());
    env.view.update_search();
    // All items should still be in flat_items
    assert_eq!(env.view.flat_items.len(), original_len);
}

#[test]
#[serial]
fn test_search_n_cycles_forward() {
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    let match_count = env.view.search_matches.len();
    assert!(match_count > 1);

    let first_cursor = env.view.cursor;
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert_eq!(env.view.search_match_index, 1);
    // Cursor should have moved
    assert_ne!(env.view.cursor, first_cursor);
}

#[test]
#[serial]
fn test_search_n_wraps_around() {
    let mut env = create_test_env_with_sessions(3);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    let match_count = env.view.search_matches.len();

    // Cycle through all matches to wrap
    for _ in 0..match_count {
        env.view.handle_key(key(KeyCode::Char('n')), None);
    }
    assert_eq!(env.view.search_match_index, 0);
}

#[test]
#[serial]
fn test_search_shift_n_opens_new_from_selection_not_cycle() {
    // #3038 regression guard: after a committed search, Shift+N must create a
    // new session (new-from-selection), not jump to the previous match. Before
    // #3038 the committed search shadowed Shift+N with a reverse-cycle.
    let mut env = create_test_env_with_sessions(5);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();
    assert!(env.view.search_matches.len() > 1);
    assert_eq!(env.view.search_match_index, 0);

    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert!(
        env.view.new_dialog.is_some(),
        "Shift+N opens new-from-selection during a committed search"
    );
    assert_eq!(
        env.view.search_match_index, 0,
        "Shift+N must not cycle the search backward"
    );
}

#[test]
#[serial]
fn matched_running_row_keeps_status_color_on_spinner_and_bolds() {
    // #3038 follow-up: a search match must not recolor the status spinner. A
    // running match used to paint its spinner theme.search (amber in most
    // themes), which read as "waiting" while the true status was running. The
    // spinner and title must keep the running status color and highlight with
    // bold only.
    use ratatui::style::Modifier;

    let (env, running, _waiting) = attention_env_running_then_waiting();
    let theme = crate::tui::styles::load_theme_with_mode("empire", false);
    assert_ne!(
        theme.running, theme.search,
        "test needs distinct running vs search colors to be meaningful"
    );

    let item = env.view.flat_items[running].clone();
    let line = env.view.render_item_line(&item, false, true, &theme, 80);

    // Spans: [indent, spinner, title, ...].
    let spinner = &line.spans[1];
    let title = &line.spans[2];

    assert_eq!(
        spinner.style.fg,
        Some(theme.running),
        "matched spinner must stay the running status color, not theme.search"
    );
    assert!(
        spinner.style.add_modifier.contains(Modifier::BOLD),
        "matched spinner should highlight with bold"
    );
    assert_eq!(
        title.style.fg,
        Some(theme.running),
        "matched title keeps the running status color"
    );
    assert!(
        title.style.add_modifier.contains(Modifier::BOLD),
        "matched title should highlight with bold"
    );
}

#[test]
#[serial]
fn test_esc_clears_search_matches() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    assert!(!env.view.search_matches.is_empty());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.search_matches.is_empty());
    assert_eq!(env.view.search_match_index, 0);
}

#[test]
#[serial]
fn committed_search_keeps_bar_visible_until_esc() {
    // The searched text should stay pinned at the bottom of the list after you
    // press Enter, until you Esc out. `search_bar_visible` gates both the render
    // and the list-row reservation, so it must stay true through a commit.
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    assert!(env.view.search_active);
    assert!(env.view.search_bar_visible());

    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.search_active, "Enter commits the search");
    assert!(!env.view.search_matches.is_empty(), "matches are kept");
    assert!(
        env.view.search_bar_visible(),
        "the committed bar stays visible so the query is still shown"
    );
    assert_eq!(
        env.view.search_query.value(),
        "s",
        "the searched text persists in the bar"
    );

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(
        !env.view.search_bar_visible(),
        "Esc clears the search and hides the bar"
    );
}

#[test]
#[serial]
fn committed_zero_result_search_keeps_bar_visible() {
    // A committed search that matched nothing is still something you searched
    // for: the bar must stay visible (showing `/query [0/0]`) until Esc, rather
    // than vanishing the instant you press Enter. Gating on the committed query
    // rather than on matches keeps it visible.
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    for ch in ['z', 'q', 'x', 'w', 'v'] {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
    assert!(
        env.view.search_matches.is_empty(),
        "the query is expected to match no session"
    );

    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(
        !env.view.search_active,
        "Enter commits even with no matches"
    );
    assert!(env.view.search_matches.is_empty());
    assert!(
        env.view.search_bar_visible(),
        "a committed zero-result search keeps the bar (and query) visible"
    );
    assert_eq!(env.view.search_query.value(), "zqxwv");

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(
        !env.view.search_bar_visible(),
        "Esc clears the committed zero-result search"
    );
}

#[test]
#[serial]
fn committed_search_bar_renders_query_after_enter() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(5);
    let theme = load_theme("empire");

    let render_to_string = |view: &mut HomeView| {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    for ch in ['/', 's', 'e', 's', 's'] {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.search_active);
    assert!(
        !env.view.search_matches.is_empty(),
        "query must match a row"
    );

    let screen = render_to_string(&mut env.view);
    // The `/`-prefixed query is unique to the bar (session titles carry no
    // leading slash), so its presence proves the committed bar rendered.
    assert!(
        screen.contains("/sess"),
        "committed search bar must still render the query after Enter"
    );
}

#[test]
#[serial]
fn test_esc_clears_matches_so_n_opens_new_dialog() {
    let mut env = create_test_env_with_sessions(5);
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(!env.view.search_active);
    assert!(env.view.search_matches.is_empty());

    assert!(env.view.new_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert!(env.view.new_dialog.is_some());
}

#[test]
#[serial]
fn open_tips_dialog_opens_even_with_no_eligible_tips() {
    // No tip earned yet: "Show tips" still opens the overlay (an empty state)
    // rather than silently doing nothing.
    let mut env = create_test_env_empty();
    assert!(env.view.tips_dialog.is_none());
    env.view.open_tips_dialog();
    assert!(env.view.tips_dialog.is_some());
}

#[test]
#[serial]
fn persist_tips_outcome_merges_seen_sets_disabled_and_updates_badge() {
    use crate::tui::dialogs::TipsOutcome;

    let mut env = create_test_env_empty();
    earn_tip(&mut env);
    let before = env.view.tips_unseen;
    assert!(before > 0);

    env.view.persist_tips_outcome(TipsOutcome {
        newly_seen: vec!["new-from-selection".to_string()],
        disabled: Some(true),
    });

    let config = crate::session::config::load_config()
        .unwrap()
        .unwrap_or_default();
    assert!(config
        .app_state
        .tips_seen
        .iter()
        .any(|s| s == "new-from-selection"));
    assert!(!config.session.show_tips);
    // Disabling tips zeroes the cached badge count.
    assert_eq!(env.view.tips_unseen, 0);
}

#[test]
#[serial]
fn tips_badge_renders_with_count_and_hides_when_zero() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");

    let render = |env: &mut TestEnv| -> String {
        let backend = TestBackend::new(200, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    // Earn the tip so the badge shows a count.
    earn_tip(&mut env);
    let n = env.view.tips_unseen;
    assert!(n > 0);
    let shown = render(&mut env);
    assert!(
        shown.contains(&format!("{n} tips")),
        "badge should show the unseen count\n{shown}"
    );

    // Zero unseen (or disabled) hides the badge entirely.
    env.view.tips_unseen = 0;
    let hidden = render(&mut env);
    assert!(
        !hidden.contains("tips"),
        "no badge when nothing is unseen\n{hidden}"
    );
}

#[test]
#[serial]
fn footer_hints_yield_to_tips_badge_when_thin() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");
    earn_tip(&mut env);
    let n = env.view.tips_unseen;
    assert!(n > 0);
    let badge = format!("{n} tips");

    let render_at = |env: &mut TestEnv, w: u16| -> String {
        let backend = TestBackend::new(w, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                env.view.render(f, area, &theme, None, None, None);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    };

    // Wide: the badge and even a low-priority hint (Diff) both fit.
    let wide = render_at(&mut env, 200);
    assert!(wide.contains(&badge), "badge shows when wide\n{wide}");
    assert!(
        wide.contains("Diff"),
        "low-priority hint present when wide\n{wide}"
    );

    // Thin: the badge still shows (it takes priority); the hints yield.
    let thin = render_at(&mut env, 30);
    assert!(
        thin.contains(&badge),
        "badge survives on a thin footer\n{thin}"
    );
    assert!(
        !thin.contains("Diff"),
        "low-priority hints drop to make room for the badge\n{thin}"
    );
}

#[test]
#[serial]
fn clicking_footer_tips_badge_opens_overlay() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");
    earn_tip(&mut env);
    assert!(env.view.tips_unseen > 0);

    // Render once so the footer captures the badge's clickable rect.
    let backend = TestBackend::new(200, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let rect = env
        .view
        .tips_badge_rect
        .expect("badge rect should be captured when shown");

    assert!(env.view.tips_dialog.is_none());
    let handled = env.view.handle_tips_badge_click(rect.x, rect.y);
    assert!(handled, "click on the badge is handled");
    assert!(
        env.view.tips_dialog.is_some(),
        "clicking the badge opens the tips overlay"
    );
}

#[test]
#[serial]
fn hovering_footer_tips_badge_sets_hover_state() {
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_with_sessions(1);
    let theme = load_theme("empire");
    earn_tip(&mut env);

    // Render once so the badge's rect is captured.
    let backend = TestBackend::new(200, 40);
    let mut terminal = Terminal::new(backend).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            env.view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let rect = env.view.tips_badge_rect.expect("badge rect captured");

    assert!(!env.view.tips_badge_hovered);
    // Hovering the badge sets the highlight and reports a change.
    assert!(env.view.handle_hover(rect.x, rect.y));
    assert!(env.view.tips_badge_hovered);
    // Moving off clears it.
    assert!(env.view.handle_hover(0, 0));
    assert!(!env.view.tips_badge_hovered);
}

#[test]
#[serial]
fn earned_new_from_selection_tip_pops_after_repeated_n_with_selection() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id);
    let before = env.view.tips_unseen;

    // Open + cancel `n` with a selection enough times to earn the tip.
    for _ in 0..crate::tips::NEW_FROM_SELECTION_TIP_THRESHOLD {
        env.view.handle_key(key(KeyCode::Char('n')), None);
        assert!(
            env.view.new_dialog.is_some(),
            "n opens the new-session dialog"
        );
        env.view.handle_key(key(KeyCode::Esc), None);
    }

    // The earned tip is now in the badge and queued to pop.
    assert_eq!(
        env.view.tips_unseen,
        before + 1,
        "earned tip joins the badge"
    );
    assert!(
        env.view.pending_tip_pop.is_some(),
        "earned tip should be queued after the threshold"
    );

    // The next idle keystroke drains the queue into the tips overlay.
    assert!(env.view.tips_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('j')), None);
    assert!(
        env.view.tips_dialog.is_some(),
        "queued earned tip should pop on the next keystroke"
    );
    assert!(env.view.pending_tip_pop.is_none(), "pop is drained once");
}

#[test]
#[serial]
fn earned_tip_does_not_pop_when_tips_disabled() {
    use crate::tui::dialogs::TipsOutcome;

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id);
    env.view.persist_tips_outcome(TipsOutcome {
        newly_seen: vec![],
        disabled: Some(true),
    });

    for _ in 0..crate::tips::NEW_FROM_SELECTION_TIP_THRESHOLD {
        env.view.handle_key(key(KeyCode::Char('n')), None);
        env.view.handle_key(key(KeyCode::Esc), None);
    }

    assert!(
        env.view.pending_tip_pop.is_none(),
        "disabled tips must not queue a pop"
    );
    assert_eq!(env.view.tips_unseen, 0, "disabled tips => empty badge");
}

#[test]
#[serial]
fn using_n_suppresses_the_earned_tip() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id);
    // Earn the tip (badge showing) without queueing a pop.
    earn_tip(&mut env);
    let earned = env.view.tips_unseen;
    assert!(earned > 0, "tip is earned and badged");

    // The user discovers N for themselves: open new-from-selection.
    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert!(
        env.view.new_dialog.is_some(),
        "N opens the new-from-selection dialog"
    );
    // The earned tip drops from the badge (rotation tips, if any, remain).
    assert_eq!(
        env.view.tips_unseen,
        earned - 1,
        "using N suppresses the tip that teaches it"
    );

    let config = crate::session::config::load_config()
        .unwrap()
        .unwrap_or_default();
    assert!(
        config.app_state.used_new_from_selection,
        "N use is persisted"
    );
}

#[test]
#[serial]
fn test_reload_does_not_snap_cursor_after_enter() {
    let mut env = create_test_env_with_sessions(5);
    // Search and commit with Enter: matches stay non-empty so
    // `refresh_search_matches` fires on reload; the cursor must not snap.
    env.view.handle_key(key(KeyCode::Char('/')), None);
    env.view.handle_key(key(KeyCode::Char('s')), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.search_active);

    // Navigate away from the search result
    env.view.cursor = 4;
    env.view.update_selected();

    // Simulate periodic reload
    env.view.reload().unwrap();

    // Cursor should stay where the user put it, not snap back to best match
    assert_eq!(env.view.cursor, 4);
}

#[test]
#[serial]
fn test_cursor_moves_over_full_list_during_search() {
    let mut env = create_test_env_with_sessions(10);
    env.view.search_query = Input::new("session".to_string());
    env.view.update_search();

    // Cursor should be able to move to last item in full list
    env.view.cursor = 0;
    for _ in 0..20 {
        env.view.move_cursor(1);
    }
    assert_eq!(env.view.cursor, 9); // last item in 10-item list
}

#[test]
#[serial]
fn test_r_opens_rename_dialog() {
    let mut env = create_test_env_with_sessions(3);
    env.view.update_selected();
    assert!(env.view.rename_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('r')), None);
    assert!(env.view.rename_dialog.is_some());
}

#[test]
#[serial]
fn test_rename_dialog_opened_on_group() {
    let mut env = create_test_env_with_groups();
    env.view.cursor = 1;
    env.view.update_selected();
    assert!(env.view.selected_group.is_some());
    assert!(env.view.rename_dialog.is_none());
    env.view.handle_key(key(KeyCode::Char('r')), None);
    assert!(env.view.rename_dialog.is_some());
    assert!(env.view.group_rename_context.is_some());
}

#[test]
#[serial]
fn test_has_dialog_returns_true_for_rename_dialog() {
    let mut env = create_test_env_with_sessions(1);
    env.view.update_selected();
    assert!(!env.view.has_dialog());
    env.view.handle_key(key(KeyCode::Char('r')), None);
    assert!(env.view.has_dialog());
}

#[test]
#[serial]
fn test_select_session_by_id() {
    let mut env = create_test_env_with_sessions(3);
    let session_id = env.view.instance_at(1).id.clone();

    assert_eq!(env.view.cursor, 0);

    env.view.select_session_by_id(&session_id);

    assert_eq!(env.view.cursor, 1);
    assert_eq!(env.view.selected_session, Some(session_id));
}

#[test]
#[serial]
fn test_select_session_by_id_nonexistent() {
    let mut env = create_test_env_with_sessions(3);

    assert_eq!(env.view.cursor, 0);
    env.view.select_session_by_id("nonexistent-id");
    assert_eq!(env.view.cursor, 0);
}

#[test]
#[serial]
fn test_select_top_attention_lands_on_first_session() {
    let mut env = create_test_env_with_sessions(3);
    env.view.cursor = 2;
    env.view.update_selected();
    assert_eq!(env.view.cursor, 2);

    env.view.select_top_attention(None);

    assert_eq!(env.view.cursor, 0);
    if let Item::Session { id, .. } = &env.view.flat_items[0] {
        assert_eq!(env.view.selected_session.as_deref(), Some(id.as_str()));
    } else {
        panic!("expected first flat_items row to be a Session");
    }
}

#[test]
#[serial]
fn test_select_top_attention_skips_returning_session() {
    let mut env = create_test_env_with_sessions(3);

    // Grab id of first session (the one we're "returning from").
    let first_id = if let Item::Session { id, .. } = &env.view.flat_items[0] {
        id.clone()
    } else {
        panic!("expected first flat_items row to be a Session");
    };
    let second_id = if let Item::Session { id, .. } = &env.view.flat_items[1] {
        id.clone()
    } else {
        panic!("expected second flat_items row to be a Session");
    };

    env.view.cursor = 0;
    env.view.update_selected();

    // Simulate returning from `first_id`: skip it, land on the next session.
    env.view.select_top_attention(Some(&first_id));

    assert_eq!(env.view.cursor, 1);
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(second_id.as_str())
    );
}

#[test]
#[serial]
fn test_select_top_attention_falls_back_to_returning_when_only_session() {
    let mut env = create_test_env_with_sessions(1);

    let only_id = if let Item::Session { id, .. } = &env.view.flat_items[0] {
        id.clone()
    } else {
        panic!("expected first flat_items row to be a Session");
    };

    env.view.cursor = 0;
    env.view.update_selected();

    // Only one session; skip would leave nothing; must fall back to it.
    env.view.select_top_attention(Some(&only_id));

    assert_eq!(env.view.cursor, 0);
    assert_eq!(env.view.selected_session.as_deref(), Some(only_id.as_str()));
}

#[test]
#[serial]
fn test_uppercase_p_opens_profile_picker() {
    let env = create_test_env_empty();
    let mut view = env.view;

    assert!(view.profile_picker_dialog.is_none());
    let action = view.handle_key(key(KeyCode::Char('P')), None);
    assert_eq!(action, None);
    assert!(view.profile_picker_dialog.is_some());
}

#[test]
#[serial]
fn test_uppercase_p_in_search_mode_does_not_open_picker() {
    let env = create_test_env_empty();
    let mut view = env.view;

    // Enter search mode
    view.handle_key(key(KeyCode::Char('/')), None);
    assert!(view.search_active);

    // P should be treated as search input, not open picker
    view.handle_key(key(KeyCode::Char('P')), None);
    assert!(view.profile_picker_dialog.is_none());
    assert_eq!(view.search_query.value(), "P");
}
