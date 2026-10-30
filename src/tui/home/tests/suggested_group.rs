//! The collapsed Suggested group in the Home list.

use super::*;
use crate::session::Item;

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
