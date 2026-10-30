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
    terminal_view_config();
    let (mut env, requests) = expanded_env(2);
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
