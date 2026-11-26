//! The Suggested row in the Home list and the suggestions pane it opens.

use super::*;
use crate::session::Item;
use crate::tui::suggestions::{StartAnswer, SuggestionsFeed};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn snapshot(count: usize, dismissed: &[usize]) -> serde_json::Value {
    let items: Vec<_> = (0..count)
        .map(|i| {
            serde_json::json!({
                "provider_id": "gh", "id": format!("i{i}"), "title": format!("Item {i}"),
                "reason": "review requested", "source_url": "https://example.test",
                "source_ref": format!("#{i}"), "preview": "", "dismissed": dismissed.contains(&i),
                "draft": { "prompt": "do it" }
            })
        })
        .collect();
    serde_json::json!({
        "providers": [{ "id": "gh", "name": "GitHub" }],
        "items": items,
        "prefs": { "sidebar": true }
    })
}

fn suggested_rows(env: &TestEnv) -> Vec<&Item> {
    env.view
        .flat_items
        .iter()
        .filter(|item| matches!(item, Item::Suggested { .. }))
        .collect()
}

fn select_suggested_row(env: &mut TestEnv) {
    env.view.cursor = env.view.suggested_row().expect("Suggested row");
    env.view.update_selected();
}

fn pane_notice(env: &TestEnv) -> Option<String> {
    env.view.suggestions_view.as_ref()?.notice.clone()
}

fn pane_error(env: &TestEnv) -> Option<String> {
    env.view.suggestions_view.as_ref()?.error.clone()
}

#[test]
#[serial]
fn the_row_is_absent_without_suggestions_and_counts_the_listed_ones() {
    let mut env = create_test_env_with_sessions(1);
    assert!(suggested_rows(&env).is_empty());

    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(5, &[1]));
    env.view.rebuild_flat_items();
    let rows = suggested_rows(&env);
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        rows[0],
        Item::Suggested {
            total: 4,
            failed: false
        }
    ));

    env.view
        .suggestions_feed
        .set_snapshot_for_test(serde_json::json!({ "providers": [], "items": [] }));
    env.view.rebuild_flat_items();
    assert!(suggested_rows(&env).is_empty());
}

/// With the Home row off, the pane says so and `h` turns it on.
#[test]
#[serial]
fn h_in_the_pane_turns_the_home_row_on() {
    let (mut env, requests) = expanded_env(2);
    let mut hidden = snapshot(2, &[]);
    hidden["prefs"]["sidebar"] = false.into();
    env.view.suggestions_feed.set_snapshot_for_test(hidden);
    env.view.focus_suggestions();
    let text = render_home_to_string(&mut env.view, 120, 40);
    assert!(
        text.contains("Not shown on Home. h shows the Suggested row there."),
        "{text}"
    );
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('h')), None);
    });
    assert_eq!(next_request(&requests), "show on home true");
}

/// The dashboard's "Show in the sidebar" pref also hides the Home row.
/// The row opens the pinned bottom shelf, directly above the Archived section
/// (expanded or collapsed), or above Trash when nothing is archived.
#[test]
#[serial]
fn the_row_sits_directly_above_archived_and_trash() {
    use crate::session::config::GroupByMode;
    use crate::session::{is_within_archived_section, is_within_trash_section};
    // (archived, archived section collapsed, trashed)
    let cases = [
        (false, false, false),
        (false, false, true),
        (true, false, false),
        (true, true, false),
        (true, false, true),
        (true, true, true),
    ];
    for group_by in [
        GroupByMode::Manual,
        GroupByMode::Project,
        GroupByMode::Status,
    ] {
        for (archived, collapsed, trashed) in cases {
            let case =
                format!("{group_by:?} archived={archived} collapsed={collapsed} trashed={trashed}");
            let mut env = create_test_env_with_sessions(3);
            env.view
                .suggestions_feed
                .set_snapshot_for_test(snapshot(2, &[]));
            env.view.group_by = group_by;
            let ids: Vec<String> = env.view.instances().map(|i| i.id.clone()).collect();
            if archived {
                env.view.mutate_instance(&ids[0], |inst| inst.archive());
            }
            if trashed {
                env.view.mutate_instance(&ids[1], |inst| {
                    inst.trashed_at = Some(crate::time::Timestamp::now())
                });
            }
            env.view.archived_section_collapsed = collapsed;
            env.view.trashed_section_collapsed = false;
            env.view.rebuild_flat_items();

            let row = env.view.suggested_row().expect(&case);
            assert_eq!(env.view.shelf_start(), Some(row), "{case}");
            assert!(
                env.view.flat_items[..row]
                    .iter()
                    .any(|item| matches!(item, Item::Session { .. })),
                "{case}: live sessions come first"
            );
            let next = env.view.flat_items.get(row + 1);
            let next_path = match next {
                Some(Item::Group { path, .. }) => Some(path.as_str()),
                _ => None,
            };
            match (archived, trashed) {
                (true, _) => assert!(next_path.is_some_and(is_within_archived_section), "{case}"),
                (false, true) => assert!(next_path.is_some_and(is_within_trash_section), "{case}"),
                (false, false) => assert!(next.is_none(), "{case}: the row is last"),
            }
            if archived && trashed {
                let trash = env
                    .view
                    .flat_items
                    .iter()
                    .position(|item| {
                        matches!(item, Item::Group { path, .. } if is_within_trash_section(path))
                    })
                    .expect(&case);
                assert!(trash > row + 1, "{case}: Trash stays below Archived");
            }
        }
    }
}

#[test]
#[serial]
fn the_row_follows_the_sidebar_pref() {
    let mut env = create_test_env_with_sessions(1);
    let mut snap = snapshot(2, &[]);
    snap["prefs"]["sidebar"] = false.into();
    env.view.suggestions_feed.set_snapshot_for_test(snap);
    env.view.rebuild_flat_items();
    assert!(suggested_rows(&env).is_empty());
}

/// Selecting the row shows the list in the preview pane without taking the keys.
#[test]
#[serial]
fn selecting_the_row_shows_the_list_in_the_preview_pane() {
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.rebuild_flat_items();
    select_suggested_row(&mut env);

    assert!(env.view.selected_session.is_none());
    assert!(env.view.selected_group.is_none());
    assert!(!env.view.suggestions_focused);
    let text = render_home_to_string(&mut env.view, 140, 40);
    for expected in [
        "Suggested sessions",
        "Item 0",
        "Item 1",
        "do it",
        "↵ browse",
        "↵ Browse",
    ] {
        assert!(text.contains(expected), "{expected:?}: {text}");
    }
    let row = env.view.cursor;
    env.view.handle_key(key(KeyCode::Char('k')), None);
    assert_ne!(env.view.cursor, row, "Home's list keeps the keys");
    assert!(!env.view.suggestions_pane_shown());
}

/// Enter or Right on the row moves the keys into the pane; Esc gives them back.
#[test]
#[serial]
fn enter_or_right_focuses_the_pane_and_esc_returns_to_the_row() {
    for focus in [KeyCode::Enter, KeyCode::Right] {
        let mut env = create_test_env_with_sessions(1);
        env.view
            .suggestions_feed
            .set_snapshot_for_test(snapshot(3, &[]));
        env.view.rebuild_flat_items();
        select_suggested_row(&mut env);
        let row = env.view.cursor;

        env.view.handle_key(key(focus), None);
        assert!(env.view.suggestions_focused, "{focus:?}");
        let text = render_home_to_string(&mut env.view, 140, 40);
        let footer = text.lines().last().unwrap_or_default();
        assert!(footer.contains("Esc Sessions"), "{footer}");
        assert!(
            !footer.contains("New"),
            "Home keys stay off the footer: {footer}"
        );
        env.view.handle_key(key(KeyCode::Char('j')), None);
        env.view.handle_key(key(KeyCode::Char('j')), None);
        let view = env.view.suggestions_view.as_ref().unwrap();
        assert_eq!(view.selected_key(), Some(("gh".into(), "i2".into())));
        assert_eq!(env.view.cursor, row, "the list's cursor stays on the row");

        env.view.handle_key(key(KeyCode::Esc), None);
        assert!(!env.view.suggestions_focused);
        assert_eq!(env.view.cursor, row);
        assert!(env.view.suggestions_pane_shown());
        let view = env.view.suggestions_view.as_ref().unwrap();
        assert_eq!(
            view.selected_key(),
            Some(("gh".into(), "i2".into())),
            "the pane keeps its selection"
        );
        env.view.handle_key(key(KeyCode::Char('k')), None);
        assert_ne!(env.view.cursor, row, "Home's list has the keys again");
    }
}

/// A remapped `activate` focuses the pane from the row, its hint names the new key,
/// and bare Enter no longer does.
#[test]
#[serial]
fn remapped_activate_focuses_the_pane() {
    use crate::tui::home::keybinds;
    let mut env = create_test_env_with_sessions(1);
    let config = crate::session::Config {
        keybinds: [("activate".to_string(), vec!["C-Enter".to_string()])].into(),
        ..Default::default()
    };
    let (keymap, warnings) = keybinds::from_config(&config);
    assert!(warnings.is_empty(), "{warnings:?}");
    keybinds::install(keymap);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.rebuild_flat_items();
    select_suggested_row(&mut env);

    let text = render_home_to_string(&mut env.view, 140, 40);
    assert!(text.contains("Ctrl+Enter browse"), "{text}");
    assert!(!text.contains("↵ browse"), "{text}");
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(!env.view.suggestions_focused, "bare Enter is unbound");
    env.view
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL), None);
    assert!(env.view.suggestions_focused);
    keybinds::install(keybinds::Keymap::default());
}

/// S (Ctrl+S in strict mode) moves the cursor to the Suggested row and
/// focuses its pane; with the row hidden it focuses the pane where the cursor
/// is, and Esc gives the preview back to that Session.
#[test]
#[serial]
fn s_selects_the_row_and_focuses_the_pane() {
    for (strict, shortcut, shown) in [
        (false, key(KeyCode::Char('S')), true),
        (
            true,
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            true,
        ),
        (false, key(KeyCode::Char('S')), false),
    ] {
        let mut env = create_test_env_with_sessions(2);
        env.view.strict_hotkeys = strict;
        let mut snap = snapshot(2, &[]);
        snap["prefs"]["sidebar"] = shown.into();
        env.view.suggestions_feed.set_snapshot_for_test(snap);
        env.view.rebuild_flat_items();
        env.view.cursor = 0;
        env.view.update_selected();
        let session = env.view.selected_session.clone();

        env.view.handle_key(shortcut, None);
        assert!(
            env.view.suggestions_focused,
            "strict {strict}, shown {shown}"
        );
        assert_eq!(env.view.on_suggested_row(), shown);
        assert!(env.view.suggestions_pane_shown());
        assert_eq!(env.view.suggestions_view.as_ref().unwrap().strict, strict);

        env.view.handle_key(key(KeyCode::Esc), None);
        assert!(!env.view.suggestions_focused);
        assert_eq!(env.view.suggestions_pane_shown(), shown);
        if !shown {
            assert_eq!(env.view.cursor, 0);
            assert_eq!(env.view.selected_session, session);
        }
    }
}

/// A click on the shown pane focuses it; a click in the list gives the list
/// the keys back.
#[test]
#[serial]
fn clicking_the_pane_focuses_it_and_clicking_the_list_returns() {
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.rebuild_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    render_home_to_string(&mut env.view, 140, 40);
    let preview = env.view.preview_area;
    assert!(
        !env.view
            .handle_suggestions_pane_click(preview.x + 2, preview.y + 2),
        "the pane is not shown on a Session"
    );

    select_suggested_row(&mut env);
    render_home_to_string(&mut env.view, 140, 40);
    let preview = env.view.preview_area;
    assert!(env
        .view
        .handle_suggestions_pane_click(preview.x + 2, preview.y + 2));
    assert!(env.view.suggestions_focused);

    let list = env.view.list_inner_area;
    let session_row = (list.y..list.y + list.height)
        .find(|&r| {
            env.view
                .resolve_row_to_index(list.x + 2, r)
                .is_some_and(|i| matches!(env.view.flat_items[i], Item::Session { .. }))
        })
        .expect("a session row on screen");
    env.view.handle_click(list.x + 2, session_row);
    assert!(!env.view.suggestions_focused);
    assert!(!env.view.suggestions_pane_shown());
}

/// Moves the keys back to the list and the cursor onto the Session above the row.
fn leave_the_pane(env: &mut TestEnv) {
    env.view.handle_key(key(KeyCode::Esc), None);
    env.view.handle_key(key(KeyCode::Char('k')), None);
    assert!(!env.view.suggestions_pane_shown());
}

/// A start answered after leaving the pane shows on the status line.
#[test]
#[serial]
fn a_start_answered_after_leaving_the_pane_flashes_on_home() {
    let (mut env, requests) = expanded_env(2);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    leave_the_pane(&mut env);
    poll_until(&mut env, |env| {
        env.view
            .status_flash_text()
            .is_some_and(|text| text.contains("Started Prepared title"))
    });
    let flash = env.view.status_flash_text().unwrap();
    assert!(
        !flash.contains("Enter opens"),
        "Enter on Home opens nothing: {flash}"
    );
}

/// A prefill answered after leaving the pane does not open the dialog over
/// Home, and says so there.
#[test]
#[serial]
fn a_prepare_answered_after_leaving_the_pane_opens_nothing() {
    let (mut env, requests) = expanded_env(2);
    terminal_view_config();
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('e')), None);
    });
    assert_eq!(next_request(&requests), "prepare gh/i0");
    leave_the_pane(&mut env);
    poll_until(&mut env, |env| {
        env.view
            .status_flash_text()
            .is_some_and(|text| text.starts_with("Not opened"))
    });
    assert!(env.view.new_dialog.is_none());
}

/// A dialog that opens over the focused pane takes the keys first.
#[test]
#[serial]
fn a_dialog_over_the_focused_pane_takes_the_keys() {
    let (mut env, _requests) = expanded_env(2);
    env.view.info_dialog = Some(crate::tui::dialogs::InfoDialog::new("Note", "hello"));
    env.view.handle_key(key(KeyCode::Char('j')), None);
    let view = env.view.suggestions_view.as_ref().unwrap();
    assert_eq!(view.selected_key(), Some(("gh".into(), "i0".into())));
}

/// Focusing the pane releases a focused Trash transcript reader, so the pane
/// gets the keys at once and one Esc leaves it.
#[test]
#[serial]
fn focusing_the_pane_releases_a_focused_trash_reader() {
    let mut env = create_test_env_with_sessions(2);
    let id = env.view.instance_at(0).id.clone();
    env.view.mutate_instance(&id, |inst| {
        inst.trashed_at = Some(crate::time::Timestamp::now())
    });
    env.view.trashed_section_collapsed = false;
    let mut hidden = snapshot(3, &[]);
    hidden["prefs"]["sidebar"] = false.into();
    env.view.suggestions_feed.set_snapshot_for_test(hidden);
    env.view.rebuild_flat_items();
    env.view.select_session_by_id(&id);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.retained_reader_focused());

    env.view.focus_suggestions();
    assert!(!env.view.retained_reader_focused());
    env.view.handle_key(key(KeyCode::Char('j')), None);
    let view = env.view.suggestions_view.as_ref().unwrap();
    assert_eq!(view.selected_key(), Some(("gh".into(), "i1".into())));
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(!env.view.suggestions_focused, "one Esc leaves the pane");
}

/// When the row goes away under the focused pane (`h` turned the pref off, or
/// the suggestions belong to another profile), the pane stays until Esc, which
/// lands on a valid neighbouring row.
#[test]
#[serial]
fn the_row_vanishing_under_the_focused_pane_leaves_a_valid_cursor() {
    for case in ["pref off", "other profile"] {
        let mut env = create_test_env_with_sessions(1);
        let mut snap = snapshot(2, &[]);
        if case == "pref off" {
            snap["prefs"]["sidebar"] = false.into();
        } else {
            snap["profile"] = "theirs".into();
        }
        // A reload racing the test answers with the case's snapshot too.
        let (sink, _requests) = mpsc::channel();
        env.view.suggestions_feed = SuggestionsFeed::recording_for_test(snap.clone(), sink);
        env.view
            .suggestions_feed
            .set_snapshot_for_test(snapshot(2, &[]));
        env.view.rebuild_flat_items();
        select_suggested_row(&mut env);
        env.view.handle_key(key(KeyCode::Enter), None);
        assert!(env.view.suggestions_focused, "{case}");
        if case == "other profile" {
            let mine = env.view.instance_at(0).source_profile.clone();
            env.view.active_profile = Some(mine);
        }
        env.view.suggestions_feed.set_snapshot_for_test(snap);
        env.view.apply_suggestions_results();
        // What a poll that brings this snapshot does.
        env.view.rebuild_flat_items_keeping_cursor();
        env.view.update_selected();
        assert!(suggested_rows(&env).is_empty(), "{case}");
        assert!(env.view.suggestions_pane_shown(), "{case}: the pane stays");

        env.view.handle_key(key(KeyCode::Esc), None);
        assert!(!env.view.suggestions_pane_shown(), "{case}");
        let Some(Item::Session { id, .. }) = env.view.flat_items.get(env.view.cursor) else {
            panic!("{case}: the cursor is on no session row");
        };
        assert_eq!(
            env.view.selected_session.as_deref(),
            Some(id.as_str()),
            "{case}"
        );
    }
}

/// Home's own keys do nothing to Home while the pane has the keys.
#[test]
#[serial]
fn home_keys_do_not_reach_home_while_the_pane_is_focused() {
    let (mut env, _requests) = expanded_env(2);
    let (cursor, width) = (env.view.cursor, env.view.list_width);
    for code in [
        KeyCode::Char('n'),
        KeyCode::Char('d'),
        KeyCode::Char('D'),
        KeyCode::Delete,
        KeyCode::Char('l'),
    ] {
        with_authenticated_local_project_registry(&mut env.view, |view| {
            view.handle_key(key(code), None);
        });
        assert!(env.view.suggestions_focused, "{code:?}");
        assert!(
            !env.view.has_non_live_send_overlay(),
            "{code:?} opened a dialog"
        );
        assert!(env.view.new_dialog.is_none(), "{code:?}");
        assert_eq!(
            (env.view.cursor, env.view.list_width),
            (cursor, width),
            "{code:?}"
        );
    }
}

/// Strict mode moves the pane's letters: `E`, `Ctrl+N`, `H` and `R` act, and
/// the bare letters before them do nothing.
#[test]
#[serial]
fn strict_mode_pane_keys_take_shift_or_ctrl() {
    for (press, expected) in [
        (key(KeyCode::Char('E')), "prepare gh/i0"),
        (
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            "prepare gh/i0",
        ),
        (key(KeyCode::Char('H')), "show on home false"),
        // `R` asks providers to refresh, which is not recorded, then reloads.
        (key(KeyCode::Char('R')), "load"),
    ] {
        let (mut env, requests) = expanded_env(2);
        env.view.strict_hotkeys = true;
        env.view.ensure_suggestions_view();
        with_authenticated_local_project_registry(&mut env.view, |view| {
            for bare in ['e', 'N', 'h'] {
                view.handle_key(key(KeyCode::Char(bare)), None);
            }
            view.handle_key(press, None);
        });
        // A queued request goes out when a poll takes the one before it.
        let deadline = Instant::now() + Duration::from_secs(5);
        let sent = loop {
            if let Ok(request) = requests.try_recv() {
                break request;
            }
            env.view.apply_suggestions_results();
            assert!(Instant::now() < deadline, "{press:?}: nothing sent");
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(sent, expected, "{press:?}");
    }
}

/// On the unfocused row Ctrl+K opens Home's palette, which has no source entry.
#[test]
#[serial]
fn ctrl_k_on_the_unfocused_row_opens_homes_palette_without_the_source() {
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(1, &[]));
    env.view.rebuild_flat_items();
    select_suggested_row(&mut env);
    env.view.handle_key(ctrl_k(), None);
    let palette = env.view.command_palette.as_ref().expect("Home's palette");
    let ids = palette.entry_ids();
    assert!(ids.contains(&"suggestions"), "{ids:?}");
    assert!(!ids.contains(&"open-suggestion-source"), "{ids:?}");
}

fn expanded_env(count: usize) -> (TestEnv, mpsc::Receiver<String>) {
    let started = StartAnswer::Started {
        session_id: "s-new".into(),
        title: "Prepared title".into(),
    };
    expanded_env_answering(count, started)
}

/// Home with the cursor on the Suggested row and its pane focused; the
/// suggestions worker records its requests and answers a start with `started`.
fn expanded_env_answering(count: usize, started: StartAnswer) -> (TestEnv, mpsc::Receiver<String>) {
    let mut env = create_test_env_with_sessions(1);
    let (sink, requests) = mpsc::channel();
    env.view.suggestions_feed =
        SuggestionsFeed::recording_start_for_test(snapshot(count, &[]), sink, started);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(count, &[]));
    env.view.rebuild_flat_items();
    select_suggested_row(&mut env);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.suggestions_focused);
    (env, requests)
}

fn next_request(requests: &mpsc::Receiver<String>) -> String {
    requests
        .recv_timeout(Duration::from_secs(5))
        .expect("a request reaches the worker")
}

fn poll_until(env: &mut TestEnv, done: impl Fn(&TestEnv) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(env) {
        env.view.apply_suggestions_results();
        assert!(Instant::now() < deadline, "condition not reached in time");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn terminal_view_config() {
    // A structured default would make the dialog check for the ACP adapter on the host.
    crate::session::config::update_config(|config| {
        config.acp.structured_view = crate::session::config::StructuredView::Terminal;
    })
    .unwrap();
}

#[test]
#[serial]
fn enter_on_a_suggestion_with_no_linked_project_opens_the_prefilled_dialog() {
    let (mut env, requests) = expanded_env_answering(2, StartAnswer::NoProject);
    terminal_view_config();
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    poll_until(&mut env, |env| env.view.new_dialog.is_some());
    assert_eq!(next_request(&requests), "start gh/i0");
    assert_eq!(next_request(&requests), "prepare gh/i0");

    let data = match env
        .view
        .new_dialog
        .as_mut()
        .unwrap()
        .handle_key(key(KeyCode::Enter))
    {
        crate::tui::dialogs::DialogResult::Submit(data) => data,
        _ => panic!("expected Submit"),
    };
    assert_eq!(data.path, "/tmp");
    assert_eq!(data.title, "Prepared title");
    assert_eq!(data.initial_prompt.as_deref(), Some("Prepared prompt"));
    assert!(data.worktree_enabled, "the preset's worktree row");
    assert!(data.yolo_mode, "the preset's yolo row");
    assert_eq!(
        data.extra_repo_paths,
        ["/tmp/extra"],
        "the repo's other mapped Projects"
    );
}

#[test]
#[serial]
fn enter_starts_a_suggestion_and_the_row_stops_counting_it() {
    let (mut env, requests) = expanded_env(2);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| {
        pane_notice(env).is_some_and(|text| text == "Started Prepared title · Enter opens it")
    });
    assert_eq!(next_request(&requests), "load");
    assert!(env.view.new_dialog.is_none(), "no wizard for a plain start");
    let linked = env
        .view
        .suggestions_feed
        .started_session_for_test(&("gh".into(), "i0".into()));
    assert_eq!(linked.as_deref(), Some("s-new"));
    assert!(
        matches!(suggested_rows(&env)[..], [Item::Suggested { total: 1, .. }]),
        "the Session is listed under its Project instead"
    );
}

#[test]
#[serial]
fn a_second_enter_does_not_start_twice() {
    let (mut env, requests) = expanded_env(2);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| {
        pane_notice(env).is_some_and(|text| text.contains("Started"))
    });
    // The reload after a start follows the first start; a duplicate would come before it.
    assert_eq!(next_request(&requests), "load", "no duplicate start");
}

#[test]
#[serial]
fn a_start_the_daemon_says_is_already_started_becomes_open() {
    let answer = StartAnswer::AlreadyStarted(Some("s-other".into()));
    let (mut env, requests) = expanded_env_answering(2, answer);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| {
        pane_notice(env).is_some_and(|text| text.contains("Already started"))
    });
    let linked = env
        .view
        .suggestions_feed
        .started_session_for_test(&("gh".into(), "i0".into()));
    assert_eq!(linked.as_deref(), Some("s-other"));
}

#[test]
#[serial]
fn hooks_that_need_trust_show_a_review_and_approving_starts_with_its_fingerprint() {
    let answer = StartAnswer::NeedsTrust {
        mcp_summaries: vec![],
        commands: vec!["on create: make setup".into()],
        fingerprint: serde_json::json!({ "project_path": "/repo", "hooks": "abc" }),
    };
    let (mut env, requests) = expanded_env_answering(2, answer);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| env.view.confirm_dialog.is_some());
    assert_eq!(
        env.view.confirm_dialog.as_ref().unwrap().action(),
        "approve_suggestion_hooks"
    );
    env.view.handle_key(key(KeyCode::Char('y')), None);
    assert_eq!(
        next_request(&requests),
        r#"start gh/i0 {"project_path":"/repo","hooks":"abc"}"#
    );
}

#[test]
#[serial]
fn a_hooks_review_does_not_replace_an_open_dialog() {
    let answer = StartAnswer::NeedsTrust {
        mcp_summaries: vec![],
        commands: vec!["on create: make setup".into()],
        fingerprint: serde_json::json!({ "project_path": "/repo" }),
    };
    let (mut env, requests) = expanded_env_answering(2, answer);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    env.view.confirm_dialog = Some(crate::tui::dialogs::ConfirmDialog::new(
        "Prompt not delivered",
        "",
        "other",
    ));
    poll_until(&mut env, |env| pane_error(env).is_some());
    assert_eq!(env.view.confirm_dialog.as_ref().unwrap().action(), "other");
    assert!(pane_error(&env).unwrap().contains("hooks need review"));
}

#[test]
#[serial]
fn the_hooks_review_lists_the_mcp_servers_the_daemon_summarised() {
    let answer = StartAnswer::NeedsTrust {
        commands: vec!["on create: make setup".into()],
        mcp_summaries: vec!["github: npx @modelcontextprotocol/server-github".into()],
        fingerprint: serde_json::json!({ "project_path": "/repo" }),
    };
    let (mut env, requests) = expanded_env_answering(2, answer);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| env.view.confirm_dialog.is_some());
    let text = render_home_to_string(&mut env.view, 140, 40);
    assert!(text.contains("MCP server: github"), "{text}");
}

#[test]
#[serial]
fn a_changed_trust_asks_the_daemon_for_a_fresh_review_without_approval() {
    let (mut env, requests) = expanded_env_answering(2, StartAnswer::TrustChanged);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    let deadline = Instant::now() + Duration::from_secs(5);
    let second = loop {
        env.view.apply_suggestions_results();
        if let Ok(request) = requests.try_recv() {
            break request;
        }
        assert!(Instant::now() < deadline, "no second start");
        std::thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(second, "start gh/i0");
}

#[test]
#[serial]
fn a_failed_start_in_the_pane_shows_its_whole_reason() {
    let reason = format!("Docker isn't running{}. Start it.", ", really".repeat(20));
    let (mut env, requests) = expanded_env_answering(2, StartAnswer::Failed(reason));
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| env.view.suggestions_feed.failed.is_some());
    let text: String = render_home_to_string(&mut env.view, 100, 40)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    assert!(
        text.contains("really. Start it."),
        "the end of the reason: {text}"
    );
}

#[test]
#[serial]
fn the_hooks_review_opened_from_the_pane_is_drawn_and_takes_the_keys() {
    let answer = StartAnswer::NeedsTrust {
        mcp_summaries: vec![],
        commands: vec!["on create: make setup".into()],
        fingerprint: serde_json::json!({ "project_path": "/repo", "hooks": "abc" }),
    };
    let (mut env, requests) = expanded_env_answering(2, answer);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| env.view.confirm_dialog.is_some());
    let text = render_home_to_string(&mut env.view, 120, 40);
    assert!(text.contains("Review repository hooks"), "{text}");
    env.view.handle_key(key(KeyCode::Char('y')), None);
    assert_eq!(
        next_request(&requests),
        r#"start gh/i0 {"project_path":"/repo","hooks":"abc"}"#
    );
}

#[test]
#[serial]
fn cancelling_the_hooks_review_in_the_pane_says_nothing_started() {
    let answer = StartAnswer::NeedsTrust {
        mcp_summaries: vec![],
        commands: vec!["on create: make setup".into()],
        fingerprint: serde_json::json!({ "project_path": "/repo" }),
    };
    let (mut env, requests) = expanded_env_answering(2, answer);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(next_request(&requests), "start gh/i0");
    poll_until(&mut env, |env| env.view.confirm_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);
    env.view.apply_suggestions_results();

    assert!(!env.view.suggestions_feed.has_held_trust());
    let text = render_home_to_string(&mut env.view, 120, 40);
    assert!(!text.contains("Starting…"), "{text}");
    assert!(text.contains("Not started"), "{text}");
}

#[test]
#[serial]
fn a_profile_switch_closes_the_hooks_review_and_nothing_is_approved() {
    let answer = StartAnswer::NeedsTrust {
        mcp_summaries: vec![],
        commands: vec!["on create: make setup".into()],
        fingerprint: serde_json::json!({ "project_path": "/repo" }),
    };
    let (mut env, requests) = expanded_env_answering(2, answer);
    env.view.active_profile = Some("A".into());
    env.view.apply_suggestions_results();
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    poll_until(&mut env, |env| env.view.confirm_dialog.is_some());
    env.view.active_profile = Some("B".into());
    env.view.apply_suggestions_results();
    assert!(env.view.confirm_dialog.is_none(), "the review is gone");
    env.view.handle_key(key(KeyCode::Char('y')), None);
    let sent: Vec<String> = requests.try_iter().collect();
    assert!(
        sent.iter().all(|r| !r.contains("project_path")),
        "no approval was sent: {sent:?}"
    );
}

#[test]
#[serial]
fn e_in_the_pane_opens_the_prefilled_dialog_in_one_press() {
    let (mut env, requests) = expanded_env(2);
    terminal_view_config();
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('e')), None);
    });
    assert_eq!(next_request(&requests), "prepare gh/i0");
    poll_until(&mut env, |env| env.view.new_dialog.is_some());
    assert!(
        !env.view.suggestions_focused,
        "the dialog takes the keys from the pane"
    );
}

#[test]
#[serial]
fn enter_in_the_pane_on_an_unlinked_item_opens_the_dialog_to_pick_a_project() {
    let (mut env, requests) = expanded_env_answering(2, StartAnswer::NoProject);
    terminal_view_config();
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Enter), None);
    });
    poll_until(&mut env, |env| env.view.new_dialog.is_some());
    assert_eq!(next_request(&requests), "start gh/i0");
    assert_eq!(next_request(&requests), "prepare gh/i0");
}

#[test]
#[serial]
fn the_pane_dismisses_with_z_and_in_strict_mode_only_with_capital_z() {
    for (strict, dismiss, ignored) in [(false, 'z', 'Z'), (true, 'Z', 'z')] {
        let (mut env, requests) = expanded_env(2);
        env.view.strict_hotkeys = strict;
        with_authenticated_local_project_registry(&mut env.view, |view| {
            view.focus_suggestions();
            view.handle_key(key(KeyCode::Char(ignored)), None);
            view.suggestions_feed.reload();
        });
        assert_eq!(next_request(&requests), "load", "{ignored} does nothing");
        with_authenticated_local_project_registry(&mut env.view, |view| {
            view.handle_key(key(KeyCode::Char(dismiss)), None);
        });
        poll_until(&mut env, |env| {
            !env.view.suggestions_feed.is_busy_for_test()
        });
        assert_eq!(next_request(&requests), "dismiss gh/i0 true", "{dismiss}");
    }
}

#[test]
#[serial]
fn the_row_does_not_count_a_started_suggestion() {
    let mut env = create_test_env_with_sessions(1);
    let mut snap = snapshot(2, &[]);
    snap["items"][0]["started_session"] = serde_json::json!({ "id": "s1", "status": "Idle" });
    env.view.suggestions_feed.set_snapshot_for_test(snap);
    env.view.rebuild_flat_items();
    assert!(matches!(
        suggested_rows(&env)[..],
        [Item::Suggested { total: 1, .. }]
    ));
}

#[test]
#[serial]
fn z_dismisses_when_the_daemon_accepts_mutations_and_is_refused_otherwise() {
    let (mut env, requests) = expanded_env(2);

    env.view.handle_key(key(KeyCode::Char('z')), None);
    assert!(pane_error(&env).unwrap().contains("Disconnected"));
    // Requests are FIFO: a refused dismiss never reached the worker ahead of this one.
    env.view.suggestions_feed.reload();
    assert_eq!(next_request(&requests), "load");
    poll_until(&mut env, |env| {
        !env.view.suggestions_feed.is_busy_for_test()
    });

    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('z')), None);
    });
    assert_eq!(next_request(&requests), "dismiss gh/i0 true");
}

#[test]
#[serial]
fn n_on_a_suggestion_prepares_another_session_that_does_not_claim_the_link() {
    let (mut env, requests) = expanded_env(2);
    terminal_view_config();
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('N')), None);
    });
    assert_eq!(next_request(&requests), "prepare gh/i0");
    poll_until(&mut env, |env| env.view.new_dialog.is_some());
    let crate::tui::dialogs::DialogResult::Submit(data) = env
        .view
        .new_dialog
        .as_mut()
        .unwrap()
        .handle_key(key(KeyCode::Enter))
    else {
        panic!("expected Submit");
    };
    assert_eq!(data.initial_prompt.as_deref(), Some("Prepared prompt"));
    assert!(data.suggestion.is_none(), "another does not link");
}

#[test]
#[serial]
fn enter_on_an_unstarted_suggestion_is_refused_while_mutations_are_unavailable() {
    let (mut env, requests) = expanded_env(2);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(pane_error(&env).unwrap().contains("Disconnected"));
    assert!(requests.try_recv().is_err(), "no start was sent");
}

#[test]
#[serial]
fn a_second_n_does_not_queue_a_second_prepare() {
    let (mut env, requests) = expanded_env(2);
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('N')), None);
        view.handle_key(key(KeyCode::Char('N')), None);
    });
    assert_eq!(next_request(&requests), "prepare gh/i0");
    poll_until(&mut env, |env| env.view.new_dialog.is_some());
    env.view.new_dialog = None;
    // The worker is FIFO: a duplicate prepare would reach it before this load.
    env.view.suggestions_feed.reload();
    assert_eq!(next_request(&requests), "load", "no duplicate prepare");
    poll_until(&mut env, |env| {
        !env.view.suggestions_feed.is_busy_for_test()
    });
    // A duplicate answer would reopen the dialog the user just cancelled.
    assert!(env.view.new_dialog.is_none());
}

#[test]
#[serial]
fn z_in_the_pane_dismisses_then_restores_within_the_undo_window() {
    let mut env = create_test_env_with_sessions(1);
    let (sink, requests) = mpsc::channel();
    // The worker answers every request with i0 dismissed, as the daemon would after a dismiss.
    env.view.suggestions_feed = SuggestionsFeed::recording_for_test(snapshot(2, &[0]), sink);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.rebuild_flat_items();
    select_suggested_row(&mut env);
    env.view.handle_key(key(KeyCode::Enter), None);

    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('z')), None);
    });
    assert_eq!(next_request(&requests), "dismiss gh/i0 true");
    let key_i0 = ("gh".to_string(), "i0".to_string());
    poll_until(&mut env, |env| env.view.suggestions_feed.is_recent(&key_i0));
    assert_eq!(
        env.view.suggestions_view.as_ref().unwrap().selected_key(),
        Some(key_i0),
        "the dismissed item stays selected for the undo window"
    );

    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('z')), None);
    });
    assert_eq!(next_request(&requests), "dismiss gh/i0 false");
}

#[test]
#[serial]
fn status_grouping_keeps_the_suggested_row() {
    use crate::session::config::GroupByMode;
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.group_by = GroupByMode::Status;
    env.view.rebuild_flat_items();
    assert_eq!(suggested_rows(&env).len(), 1);
}

#[test]
#[serial]
fn a_failed_background_load_keeps_the_snapshot_and_stays_quiet() {
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.suggestions_feed = SuggestionsFeed::failed_load_for_test("local daemon unavailable");
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.apply_suggestions_results();
    assert!(env.view.status_flash_text().is_none());
    assert_eq!(env.view.suggestions_feed.keys().len(), 2);
}

#[test]
#[serial]
fn the_cursor_stays_on_the_row_when_the_list_rebuilds() {
    let (mut env, _requests) = expanded_env(5);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(5, &[0]));
    env.view.rebuild_flat_items_keeping_cursor();
    assert!(env.view.on_suggested_row());
    assert!(matches!(
        suggested_rows(&env)[..],
        [Item::Suggested { total: 4, .. }]
    ));
}

fn ctrl_k() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
}

/// Runs `body` with `AOE_OPEN_URL_TO` pointing at a fresh file and returns the
/// URLs recorded there.
fn recorded_opens(body: impl FnOnce()) -> Vec<String> {
    let file = tempfile::NamedTempFile::new().unwrap();
    {
        let _env = crate::session::test_support::EnvGuard::set(&[("AOE_OPEN_URL_TO", file.path())]);
        body();
    }
    std::fs::read_to_string(file.path())
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[test]
#[serial]
fn open_in_source_is_offered_for_an_http_source_only() {
    let (mut env, _requests) = expanded_env(1);
    let key = ("gh".to_string(), "i0".to_string());
    let command = env.view.open_source_command(&key).expect("an entry");
    assert_eq!(command.title, "Open in GitHub");
    assert!(matches!(
        command.payload,
        crate::tui::dialogs::PaletteAction::OpenLink(ref url) if url == "https://example.test"
    ));

    let mut hostile = snapshot(1, &[]);
    hostile["items"][0]["source_url"] = "javascript:alert(1)".into();
    env.view.suggestions_feed.set_snapshot_for_test(hostile);
    assert!(env.view.open_source_command(&key).is_none());
}

#[test]
#[serial]
fn the_full_view_palette_opens_the_selected_source_and_esc_returns_to_the_view() {
    let (mut env, _requests) = expanded_env(2);
    env.view.focus_suggestions();

    env.view.handle_key(ctrl_k(), None);
    assert!(env.view.command_palette.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.command_palette.is_none());
    assert!(env.view.suggestions_view.is_some());

    let opened = recorded_opens(|| {
        env.view.handle_key(ctrl_k(), None);
        env.view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(opened, ["https://example.test"]);
    assert!(env.view.suggestions_view.is_some());
}

#[test]
#[serial]
fn the_full_view_says_so_when_the_selected_suggestion_has_no_link() {
    let (mut env, _requests) = expanded_env(1);
    let mut hostile = snapshot(1, &[]);
    hostile["items"][0]["source_url"] = "javascript:alert(1)".into();
    env.view.suggestions_feed.set_snapshot_for_test(hostile);
    env.view.focus_suggestions();

    env.view.handle_key(ctrl_k(), None);
    assert!(env.view.command_palette.is_none());
    let error = env.view.suggestions_view.as_ref().unwrap().error.as_deref();
    assert_eq!(error, Some("This suggestion has no link to open"));
}

#[test]
#[serial]
fn the_row_counts_only_the_items_it_lists_not_stale_ones() {
    let (mut env, _requests) = expanded_env(2);
    let mut snap = snapshot(2, &[]);
    snap["items"][1]["stale"] = true.into();
    env.view.suggestions_feed.set_snapshot_for_test(snap);
    env.view.rebuild_flat_items();
    assert!(matches!(
        suggested_rows(&env)[..],
        [Item::Suggested { total: 1, .. }]
    ));
}

#[test]
#[serial]
fn ctrl_k_on_an_empty_suggestions_list_does_nothing() {
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(0, &[]));
    env.view.focus_suggestions();

    env.view.handle_key(ctrl_k(), None);
    assert!(env.view.command_palette.is_none());
    assert!(env.view.suggestions_view.as_ref().unwrap().error.is_none());
}

#[test]
#[serial]
fn clicking_the_suggested_row_leaves_live_send() {
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};
    let (mut env, _requests) = expanded_env(1);
    env.view.list_inner_area = ratatui::layout::Rect::new(1, 1, 28, 5);
    env.view.shelf_inner_area = ratatui::layout::Rect::new(1, 7, 28, 5);
    let (click_row, idx) = (1..12)
        .find_map(|r| {
            let idx = env.view.resolve_row_to_index(5, r)?;
            matches!(env.view.flat_items[idx], Item::Suggested { .. }).then_some((r, idx))
        })
        .expect("the Suggested row on screen");
    env.view.live_send = Some(LiveSendState {
        session_id: "live".to_string(),
        title: "live".to_string(),
        tmux_name: "aoe_test_suggestion_click".to_string(),
        target: LiveSendTarget::Agent,
        exit_chords: Vec::new(),
        leader: None,
    });

    env.view.handle_click(5, click_row);
    assert_eq!(env.view.cursor, idx);
    assert!(env.view.live_send.is_none());
}

#[test]
#[serial]
fn the_row_shows_without_any_session() {
    let mut env = create_test_env_with_sessions(0);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(1, &[]));
    env.view.rebuild_flat_items();
    let theme = crate::tui::styles::Theme::default();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| env.view.render(f, f.area(), &theme, None))
        .unwrap();
    let text: String = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect();
    assert!(text.contains("Suggested"), "{text}");
    assert!(!text.contains("No sessions yet"));
}

/// The tip's signal comes from the daemon's snapshot: it holds while every
/// installed source is off, and not before a snapshot loads.
#[test]
#[serial]
fn the_suggestions_tip_signal_follows_the_daemon_snapshot() {
    let mut env = create_test_env_with_sessions(1);
    let config = crate::session::Config::default();
    env.view.suggestions_feed = SuggestionsFeed::failed_load_for_test("not yet");
    assert!(!env.view.tip_signals(&config).suggestions_setup_available);
    for (enabled, expected) in [(false, true), (true, false)] {
        let mut snap = snapshot(0, &[]);
        snap["providers"][0]["enabled"] = enabled.into();
        env.view.suggestions_feed.set_snapshot_for_test(snap);
        assert_eq!(
            env.view.tip_signals(&config).suggestions_setup_available,
            expected,
            "enabled={enabled}"
        );
        // Only an eligible tip takes the pop slot, so it never blocks another one.
        env.view.pending_tip_pop = None;
        env.view.queue_suggested_sessions_tip();
        assert_eq!(
            env.view.pending_tip_pop.map(|tip| tip.id),
            expected.then_some(crate::tips::SUGGESTED_SESSIONS_TIP_ID),
            "enabled={enabled}"
        );
    }
}

#[test]
#[serial]
fn an_undelivered_first_prompt_is_announced_once_per_failure() {
    let mut env = create_test_env_with_sessions(1);
    let instance = env.view.instances().next().unwrap().clone();
    let mut row = crate::server::api::sessions::session_response_from_instance(&instance, false);
    row.initial_prompt_error = Some("the agent was not ready".into());
    let announce = |env: &mut TestEnv, row: &crate::daemon::SessionResponse| {
        env.view.announce_prompt_failure(row);
        env.view.promote_prompt_failure();
    };

    // As on launch, before the first snapshot is applied.
    env.view.prompt_failures_seeded = false;
    announce(&mut env, &row);
    assert!(
        env.view.info_dialog.is_none(),
        "a failure the first snapshot already carries is not announced"
    );
    env.view.prompt_failures_seeded = true;
    announce(&mut env, &row);
    assert!(env.view.info_dialog.is_none(), "nor on later snapshots");

    row.initial_prompt_error = None;
    announce(&mut env, &row);
    row.initial_prompt_error = Some("the agent was not ready".into());
    // The pre-input path only queues it, so a key cannot close it unseen.
    env.view.announce_prompt_failure(&row);
    assert!(env.view.info_dialog.is_none());
    // Another dialog holds it back until it closes.
    env.view.confirm_dialog = Some(crate::tui::dialogs::ConfirmDialog::new(
        "Other", "", "other",
    ));
    assert!(!env.view.promote_prompt_failure());
    env.view.confirm_dialog = None;
    assert!(env.view.promote_prompt_failure());
    assert!(env.view.info_dialog.is_some(), "a new failure is announced");
    env.view.info_dialog = None;
    announce(&mut env, &row);
    assert!(
        env.view.info_dialog.is_none(),
        "the same failure is not repeated"
    );

    // With a retained prompt, the clipboard is written only by the Copy button.
    env.view
        .instances
        .get_mut(&row.id)
        .unwrap()
        .pending_initial_turn =
        Some(serde_json::from_value(serde_json::json!({ "text": "fix the bug" })).unwrap());
    row.initial_prompt_error = None;
    announce(&mut env, &row);
    row.initial_prompt_error = Some("the agent was not ready".into());
    announce(&mut env, &row);
    let dialog = env.view.confirm_dialog.as_ref().expect("Copy is offered");
    assert_eq!(dialog.action(), "copy_undelivered_prompt");
    assert_eq!(env.view.undelivered_prompt.as_deref(), Some("fix the bug"));
}

#[test]
#[serial]
fn open_on_an_unknown_session_says_so_and_keeps_the_full_view() {
    let mut env = create_test_env_with_sessions(1);
    env.view.focus_suggestions();
    let reason = env.view.try_open_linked_session("gone").unwrap_err();
    assert!(reason.contains("no longer exists"), "{reason}");
    assert!(env.view.suggestions_view.is_some());
}

#[test]
#[serial]
fn open_reaches_a_session_in_a_collapsed_archive_project_subgroup() {
    use crate::session::config::GroupByMode;
    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    let target = env.view.instances().next().unwrap().id.clone();
    let project = crate::tui::home::project_group_key(env.view.get_instance(&target).unwrap());
    publish_canonical_rows(&mut env, &[(target.clone(), true)], 5);
    env.view.archived_section_collapsed = true;
    env.view
        .project_group_collapsed
        .insert(crate::session::archived_project_sub_path(&project), true);
    env.view.rebuild_flat_items();

    assert!(env.view.try_open_linked_session(&target).is_ok());
    assert_eq!(env.view.selected_session.as_deref(), Some(target.as_str()));
    assert!(!env.view.archived_section_collapsed);
}

#[test]
#[serial]
fn a_failed_open_in_the_full_view_is_shown_there() {
    let mut env = create_test_env_with_sessions(1);
    let mut snap = snapshot(1, &[]);
    snap["items"][0]["started_session"] = serde_json::json!({ "id": "gone", "status": "Idle" });
    env.view.suggestions_feed.set_snapshot_for_test(snap);
    env.view.focus_suggestions();
    env.view.handle_key(key(KeyCode::Enter), None);

    let shown = env.view.suggestions_view.as_ref().expect("view stays open");
    assert!(shown
        .error
        .as_deref()
        .is_some_and(|e| e.contains("no longer exists")));
}

/// A failed refresh marks the row, and the pane leads with the error, the
/// plugin's fix and the retry key, with or without items to list.
#[test]
#[serial]
fn a_failed_refresh_leads_with_the_error_and_how_to_retry() {
    for count in [0, 2] {
        let mut env = create_test_env_with_sessions(1);
        let mut snap = snapshot(count, &[]);
        snap["providers"][0]["error"] = "token expired".into();
        snap["providers"][0]["checks"] = serde_json::json!([
            { "id": "a", "label": "A", "status": "ok", "fix": "not this" },
            { "id": "t", "label": "Token", "status": "fail", "fix": "Run gh auth login" }
        ]);
        env.view.suggestions_feed.set_snapshot_for_test(snap);
        env.view.rebuild_flat_items();
        assert!(matches!(
            suggested_rows(&env)[..],
            [Item::Suggested { failed: true, .. }]
        ));
        select_suggested_row(&mut env);

        let text = render_home_to_string(&mut env.view, 160, 40);
        assert!(text.contains(&format!("Suggested ({count}) !")), "{text}");
        let error = text.find("GitHub couldn't refresh: token expired");
        let retry = text.find("r retries.");
        assert!(error.is_some() && error < retry, "{text}");
        assert!(text.contains("Run gh auth login") && !text.contains("not this"));
        assert_eq!(
            text.contains("Shown from the last refresh."),
            count > 0,
            "{count}: {text}"
        );
        assert_eq!(
            text.contains("Nothing to suggest right now."),
            count == 0,
            "{count}: {text}"
        );
    }
}

/// Home with the pane focused on a one-click started item whose Session is
/// blocked on folder trust; the suggestions worker records its requests.
fn blocked_on_folder_trust() -> (TestEnv, String, mpsc::Receiver<String>) {
    use crate::tui::session_feed::SessionFeedResult;
    let mut env = create_test_env_with_sessions(1);
    let instance = env.view.instances().next().unwrap().clone();
    let mut row = crate::server::api::sessions::session_response_from_instance(&instance, false);
    row.initial_prompt_error = Some("Claude Code is asking whether to trust this folder.".into());
    row.initial_prompt_error_code = Some("agent_folder_not_trusted".into());
    let rows = fixture_snapshot(vec![row], "test", "test", 5);
    env.view
        .session_feed
        .publish_for_test(SessionFeedResult::Snapshot(Arc::new(rows)));
    env.view.apply_session_feed();
    env.view.info_dialog = None;
    let mut snap = snapshot(1, &[]);
    snap["items"][0]["started_session"] =
        serde_json::json!({ "id": instance.id, "status": "Idle", "started_via": "start" });
    let (sink, requests) = mpsc::channel();
    env.view.suggestions_feed = SuggestionsFeed::recording_for_test(snap.clone(), sink);
    env.view.suggestions_feed.set_snapshot_for_test(snap);
    env.view.apply_suggestions_results();
    env.view.focus_suggestions();
    (env, instance.id, requests)
}

fn trashed_at(epoch: &str) -> crate::tui::session_feed::SessionCommandOutcome {
    crate::tui::session_feed::SessionCommandOutcome::Trashed(crate::daemon::MutationReceipt {
        cursor: crate::daemon::RuntimeCursor {
            epoch: epoch.into(),
            revision: 6,
        },
        outcome: crate::daemon::TrashOutcome {
            relocation: crate::daemon::TrashRelocationOutcome::Skipped,
        },
    })
}

/// Publishes the applied rows again at `revision`, confirming receipts up to it.
fn publish_revision(env: &mut TestEnv, revision: u64) {
    let mut rows = (*env.view.session_feed.applied_snapshot().unwrap()).clone();
    rows.cursor.revision = revision;
    env.view
        .session_feed
        .publish_for_test(crate::tui::session_feed::SessionFeedResult::Snapshot(
            Arc::new(rows),
        ));
}

fn apply_feed_until(env: &mut TestEnv, done: impl Fn(&TestEnv) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done(env) {
        env.view.apply_session_feed();
        assert!(Instant::now() < deadline, "condition not reached in time");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
#[serial]
fn a_prompt_blocked_on_folder_trust_turns_on_pre_trust_then_tries_again() {
    let (mut env, id, requests) = blocked_on_folder_trust();
    let text = render_home_to_string(&mut env.view, 120, 40);
    assert!(text.contains("t turn on pre-trust"), "{text}");
    let mut commands = env.view.session_feed.request_driver_for_test();
    env.view.handle_key(key(KeyCode::Char('t')), None);
    let text = render_home_to_string(&mut env.view, 120, 40);
    assert!(text.contains("Turn on pre-trust"), "{text}");
    env.view.handle_key(key(KeyCode::Char('y')), None);
    poll_until(&mut env, |env| {
        env.view
            .suggestions_view
            .as_ref()
            .is_some_and(|view| view.pretrusted)
    });
    assert!(requests.try_iter().any(|r| r == "pre-trust"));
    env.view.handle_key(key(KeyCode::Enter), None);
    let (trashed, _) = commands(Ok(trashed_at("test"))).expect("Trash was sent");
    assert_eq!(trashed, id);
    publish_revision(&mut env, 6);
    apply_feed_until(&mut env, |env| env.view.start_after_trash.is_none());
    // The start may queue behind a reload, which a poll sends on.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !requests.try_iter().any(|r| r == "start gh/i0") {
        env.view.apply_suggestions_results();
        assert!(Instant::now() < deadline, "the retry never started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// A pre-trust confirmation acts only on the daemon and profile it was opened for.
#[test]
#[serial]
fn pre_trust_is_refused_when_the_list_changes_under_its_confirmation() {
    let (mut env, _id, requests) = blocked_on_folder_trust();
    let _commands = env.view.session_feed.request_driver_for_test();
    env.view.handle_key(key(KeyCode::Char('t')), None);
    assert!(env.view.confirm_dialog.is_some());
    let mut other = snapshot(1, &[]);
    other["profile"] = "other".into();
    env.view.suggestions_feed.set_snapshot_for_test(other);
    env.view.handle_key(key(KeyCode::Char('y')), None);
    env.view.suggestions_feed.reload();
    assert_eq!(
        next_request(&requests),
        "load",
        "no pre-trust was sent first"
    );
    let error = env.view.suggestions_view.as_ref().unwrap().error.clone();
    assert!(error.is_some_and(|e| e.contains("list changed")));
}

/// While one retry waits for its Trash, a second is refused before its Trash.
#[test]
#[serial]
fn a_second_try_again_is_refused_while_one_is_in_flight() {
    let (mut env, _id, _requests) = blocked_on_folder_trust();
    env.view.suggestions_view.as_mut().unwrap().pretrusted = true;
    let mut commands = env.view.session_feed.request_driver_for_test();
    let pending = crate::tui::home::RetryAfterTrash {
        session_id: "other".into(),
        key: ("gh".into(), "other".into()),
        epoch: "test".into(),
        origin: None,
    };
    env.view.start_after_trash = Some(pending);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(
        commands(Ok(trashed_at("test"))).is_none(),
        "no second Trash"
    );
    let retry = env.view.start_after_trash.as_ref().unwrap();
    assert_eq!(retry.session_id, "other", "the first retry is kept");
    let error = env.view.suggestions_view.as_ref().unwrap().error.clone();
    assert!(error.is_some_and(|e| e.contains("still moving to Trash")));
}

#[test]
#[serial]
fn a_failed_try_again_trash_disarms_so_a_later_trash_starts_nothing() {
    let (mut env, id, requests) = blocked_on_folder_trust();
    env.view.suggestions_view.as_mut().unwrap().pretrusted = true;
    let mut commands = env.view.session_feed.request_driver_for_test();
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(commands(Err("refused".into())).is_some(), "Trash was sent");
    apply_feed_until(&mut env, |env| env.view.start_after_trash.is_none());
    let shown = env.view.suggestions_view.as_ref().unwrap();
    assert!(
        shown
            .error
            .as_deref()
            .is_some_and(|e| e.contains("nothing started")),
        "{:?}",
        shown.error
    );

    env.view.info_dialog = None;
    assert!(env.view.trash_session_by_id(&id));
    assert!(commands(Ok(trashed_at("test"))).is_some());
    publish_revision(&mut env, 6);
    apply_feed_until(&mut env, |env| env.view.session_feed.can_submit(&id));
    env.view.apply_suggestions_results();
    let sent: Vec<String> = requests.try_iter().collect();
    assert!(sent.iter().all(|r| !r.starts_with("start")), "{sent:?}");
}

#[test]
#[serial]
fn a_prepare_answer_is_applied_when_a_prompt_error_changes_in_the_same_poll() {
    let (mut env, requests) = expanded_env(2);
    terminal_view_config();
    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('e')), None);
    });
    assert_eq!(next_request(&requests), "prepare gh/i0");
    let deadline = Instant::now() + Duration::from_secs(5);
    while env.view.new_dialog.is_none() {
        // A stale entry makes every poll also see a prompt-error change.
        env.view
            .suggestions_feed
            .blocked_prompts
            .insert("gone".into(), ("blocked".into(), None));
        env.view.apply_suggestions_results();
        assert!(Instant::now() < deadline, "the prepare answer was dropped");
        std::thread::sleep(Duration::from_millis(5));
    }
}
