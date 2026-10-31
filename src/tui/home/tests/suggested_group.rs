//! The collapsed Suggested group in the Home list.

use super::*;
use crate::session::Item;
use crate::tui::suggestions::SuggestionsFeed;
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
    serde_json::json!({ "providers": [{ "id": "gh", "name": "GitHub" }], "items": items })
}

fn suggested_rows(env: &TestEnv) -> Vec<&Item> {
    env.view
        .flat_items
        .iter()
        .filter(|item| match item {
            Item::Group { path, .. } => crate::session::is_suggested_section_path(path),
            other => !matches!(other, Item::Session { .. }),
        })
        .collect()
}

fn select_suggested_header(env: &mut TestEnv) {
    env.view.cursor = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if crate::session::is_suggested_section_path(path)))
        .expect("Suggested header");
    env.view.update_selected();
}

#[test]
#[serial]
fn group_is_absent_without_providers_and_collapsed_by_default() {
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
        Item::Group {
            collapsed: true,
            session_count: 4,
            ..
        }
    ));

    env.view
        .suggestions_feed
        .set_snapshot_for_test(serde_json::json!({ "providers": [], "items": [] }));
    env.view.rebuild_flat_items();
    assert!(suggested_rows(&env).is_empty());
}

#[test]
#[serial]
fn enter_expands_to_three_items_and_a_view_all_row() {
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(5, &[]));
    env.view.rebuild_flat_items();
    select_suggested_header(&mut env);
    env.view.handle_key(key(KeyCode::Enter), None);

    let rows = suggested_rows(&env);
    assert_eq!(rows.len(), 5);
    assert!(matches!(rows[1], Item::Suggestion { id, .. } if id == "i0"));
    assert!(matches!(rows[3], Item::Suggestion { id, .. } if id == "i2"));
    assert!(matches!(rows[4], Item::SuggestionsViewAll { total: 5, .. }));

    env.view.cursor += 4;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(
        env.view.suggestions_view.is_some(),
        "View all opens the full view"
    );
}

#[test]
#[serial]
fn suggestion_rows_have_no_session_or_group_selection() {
    let mut env = create_test_env_with_sessions(1);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(2, &[]));
    env.view.rebuild_flat_items();
    select_suggested_header(&mut env);
    env.view.handle_key(key(KeyCode::Enter), None);
    env.view.cursor += 1;
    env.view.update_selected();

    assert_eq!(
        env.view.cursor_suggestion(),
        Some(("gh".to_string(), "i0".to_string()))
    );
    assert!(env.view.selected_session.is_none());
    assert!(env.view.selected_group.is_none());
}

fn expanded_env(count: usize) -> (TestEnv, mpsc::Receiver<String>) {
    let mut env = create_test_env_with_sessions(1);
    let (sink, requests) = mpsc::channel();
    env.view.suggestions_feed = SuggestionsFeed::recording_for_test(snapshot(count, &[]), sink);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(count, &[]));
    env.view.rebuild_flat_items();
    select_suggested_header(&mut env);
    env.view.handle_key(key(KeyCode::Enter), None);
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
fn enter_on_a_suggestion_opens_the_prefilled_dialog() {
    // After expanded_env: it creates the isolated home this must land in.
    let (mut env, requests) = expanded_env(2);
    terminal_view_config();
    env.view.cursor += 1;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(next_request(&requests), "prepare gh/i0");
    poll_until(&mut env, |env| env.view.new_dialog.is_some());

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
}

#[test]
#[serial]
fn a_prepare_answered_after_the_cursor_moved_is_dropped() {
    let (mut env, requests) = expanded_env(2);
    env.view.cursor += 1;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);
    env.view.cursor -= 1;
    env.view.update_selected();
    assert_eq!(next_request(&requests), "prepare gh/i0");
    poll_until(&mut env, |env| env.view.status_flash_text().is_some());
    assert!(env.view.new_dialog.is_none());
}

#[test]
#[serial]
fn z_dismisses_when_the_daemon_accepts_mutations_and_is_refused_otherwise() {
    let (mut env, requests) = expanded_env(2);
    env.view.cursor += 1;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('z')), None);
    assert!(env.view.status_flash_text().unwrap().contains("read-only"));
    // Requests are FIFO: a refused dismiss never reached the worker ahead of this one.
    env.view
        .suggestions_feed
        .prepare(("gh".into(), "i1".into()));
    assert_eq!(next_request(&requests), "prepare gh/i1");
    // Drain the prepare so the dismiss is not queued behind it.
    poll_until(&mut env, |env| {
        env.view
            .status_flash_text()
            .is_some_and(|text| text.contains("Selection changed"))
    });

    with_authenticated_local_project_registry(&mut env.view, |view| {
        view.handle_key(key(KeyCode::Char('z')), None);
    });
    assert_eq!(next_request(&requests), "dismiss gh/i0 true");
}

#[test]
#[serial]
fn n_on_a_suggestion_opens_a_blank_new_session_dialog() {
    let (mut env, _requests) = expanded_env(2);
    env.view.cursor += 1;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Char('N')), None);
    assert!(env.view.new_dialog.is_some());
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
fn the_cursor_stays_on_its_suggestion_when_the_list_reorders() {
    let (mut env, _requests) = expanded_env(5);
    env.view.cursor += 2;
    env.view.update_selected();
    assert_eq!(
        env.view.cursor_suggestion(),
        Some(("gh".to_string(), "i1".to_string()))
    );
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(5, &[0]));
    env.view.rebuild_flat_items_keeping_cursor();
    assert_eq!(
        env.view.cursor_suggestion(),
        Some(("gh".to_string(), "i1".to_string()))
    );
}

fn ctrl_k() -> KeyEvent {
    KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)
}

fn type_text(env: &mut TestEnv, text: &str) {
    for c in text.chars() {
        env.view.handle_key(key(KeyCode::Char(c)), None);
    }
}

/// Runs `body` with `AOE_OPEN_URL_TO` pointing at a fresh file and returns the
/// URLs recorded there.
fn recorded_opens(body: impl FnOnce()) -> Vec<String> {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::env::set_var("AOE_OPEN_URL_TO", file.path());
    body();
    std::env::remove_var("AOE_OPEN_URL_TO");
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
fn the_palette_opens_the_source_of_the_suggestion_under_the_cursor() {
    let (mut env, _requests) = expanded_env(2);
    env.view.cursor += 1;
    env.view.update_selected();
    let opened = recorded_opens(|| {
        env.view.handle_key(ctrl_k(), None);
        type_text(&mut env, "Open in GitHub");
        env.view.handle_key(key(KeyCode::Enter), None);
    });
    assert_eq!(opened, ["https://example.test"]);
    assert!(env.view.command_palette.is_none());
}

#[test]
#[serial]
fn the_full_view_palette_opens_the_selected_source_and_esc_returns_to_the_view() {
    let (mut env, _requests) = expanded_env(2);
    env.view.open_suggestions_view();

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
fn the_preview_key_line_names_the_source_from_120_columns() {
    let (mut env, _requests) = expanded_env(1);
    env.view.cursor += 1;
    env.view.update_selected();
    for (width, strict, expected) in [
        (119, false, "z dismiss  ^K more"),
        (120, false, "z dismiss  ^K open in GitHub"),
        (120, true, "Z dismiss  ^K open in GitHub"),
    ] {
        env.view.strict_hotkeys = strict;
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30)).unwrap();
        terminal
            .draw(|frame| {
                let area = frame.area();
                assert!(env.view.render_suggestion_preview(
                    frame,
                    area,
                    &crate::tui::styles::Theme::default()
                ));
            })
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains(expected),
            "{width} cols, strict {strict}: {expected:?}"
        );
    }
}

#[test]
#[serial]
fn the_full_view_says_so_when_the_selected_suggestion_has_no_link() {
    let (mut env, _requests) = expanded_env(1);
    let mut hostile = snapshot(1, &[]);
    hostile["items"][0]["source_url"] = "javascript:alert(1)".into();
    env.view.suggestions_feed.set_snapshot_for_test(hostile);
    env.view.open_suggestions_view();

    env.view.handle_key(ctrl_k(), None);
    assert!(env.view.command_palette.is_none());
    let error = env.view.suggestions_view.as_ref().unwrap().error.as_deref();
    assert_eq!(error, Some("This suggestion has no link to open"));
}

#[test]
#[serial]
fn clicking_a_suggestion_row_leaves_live_send() {
    use crate::tui::home::live_send::{LiveSendState, LiveSendTarget};
    let (mut env, _requests) = expanded_env(1);
    env.view.list_inner_area = ratatui::layout::Rect::new(1, 1, 28, 5);
    env.view.shelf_inner_area = ratatui::layout::Rect::new(1, 7, 28, 5);
    let (click_row, idx) = (1..12)
        .find_map(|r| {
            let idx = env.view.resolve_row_to_index(5, r)?;
            matches!(env.view.flat_items[idx], Item::Suggestion { .. }).then_some((r, idx))
        })
        .expect("a suggestion row on screen");
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
fn the_group_shows_without_any_session() {
    let mut env = create_test_env_with_sessions(0);
    env.view
        .suggestions_feed
        .set_snapshot_for_test(snapshot(1, &[]));
    env.view.rebuild_flat_items();
    let theme = crate::tui::styles::Theme::default();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| env.view.render(f, f.area(), &theme, None, None, None))
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
