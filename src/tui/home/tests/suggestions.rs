//! Suggested sessions remain independent from the normal blank-session wizard.

use super::*;
use crate::tui::suggestions::{PreparedSuggestion, SuggestionIdentity, SuggestionsView};

fn prepared(path: &std::path::Path) -> PreparedSuggestion {
    PreparedSuggestion {
        project_path: path.to_string_lossy().into_owned(),
        title: "Review change".into(),
        prompt: "Inspect the actual diff".into(),
        source_ref: "github:repo:pr:42".into(),
        suggestion: SuggestionIdentity {
            provider_id: "github".into(),
            id: "review:42".into(),
            revision: 7,
        },
    }
}

#[test]
#[serial]
fn suggestion_empty_home_entry_preserves_ordinary_blank_creation() {
    let _env = crate::session::test_support::EnvGuard::unset(&[
        "AOE_DAEMON_URL",
        "AOE_DAEMON_TOKEN",
        "AOE_DAEMON_PASSPHRASE",
    ]);
    let mut env = create_test_env_empty();
    assert!(render_home_to_string(&mut env.view, 120, 40).contains("S: Suggested sessions"));
    env.view.handle_key(key(KeyCode::Char('n')), None);
    assert!(env.view.new_dialog.is_some());
    assert!(env.view.pending_suggestion_customization.is_none());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.new_dialog.is_none());
    env.view
        .set_available_tools(AvailableTools::with_tools(&[]));
    env.view.handle_key(key(KeyCode::Char('S')), None);
    assert!(env.view.suggestions_view.is_some());
    assert!(env.view.no_agents_dialog.is_none());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while env.view.suggestions_view.as_ref().unwrap().is_busy() {
        env.view.tick_dialog();
        assert!(
            std::time::Instant::now() < deadline,
            "daemon absence did not resolve"
        );
        std::thread::yield_now();
    }
    let text = render_home_to_string(&mut env.view, 120, 40);
    assert!(text.contains("Suggested sessions"));
    assert!(!text.contains("Press 'n' to create one"));
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.suggestions_view.is_none());
    env.view.strict_hotkeys = true;
    assert!(render_home_to_string(&mut env.view, 120, 40).contains("Ctrl+S: Suggested sessions"));
    let context = super::super::bindings::Ctx {
        view_mode: ViewMode::Structured,
        sort_order: env.view.sort_order,
        has_search: false,
        project_group_selected: false,
    };
    assert_eq!(
        super::super::bindings::resolve(
            &KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            true,
            &context
        ),
        Some(super::super::bindings::ActionId::Suggestions)
    );
    assert_eq!(
        super::super::bindings::resolve(&key(KeyCode::Char('S')), true, &context),
        Some(super::super::bindings::ActionId::Settings)
    );
}

#[test]
#[serial]
fn suggestion_customize_cancel_and_async_failure_retain_review_context() {
    let mut env = create_test_env_empty();
    let prepared = prepared(env._temp.path());
    let mut view = SuggestionsView::default();
    view.prepared = Some((prepared.clone(), prepared.source_ref.clone()));
    env.view.suggestions_view = Some(view);
    assert!(env.view.tick_dialog());
    assert_eq!(
        env.view.new_dialog.as_ref().unwrap().path_value(),
        prepared.project_path
    );
    assert!(env.view.pending_suggestion_customization.is_some());
    assert!(env.view.suggestions_view.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.new_dialog.is_none());
    assert!(env.view.pending_suggestion_customization.is_none());
    assert!(env.view.suggestions_view.is_some());
    env.view.suggestions_view.as_mut().unwrap().prepared =
        Some((prepared.clone(), prepared.source_ref.clone()));
    env.view.tick_dialog();
    let (tx, rx) = std::sync::mpsc::channel();
    env.view.pending_customized_session_create = Some(rx);
    env.view.new_dialog.as_mut().unwrap().set_loading(true);
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(
        env.view.new_dialog.as_ref().unwrap().is_loading(),
        "in-flight POST cannot be abandoned and accidentally retried"
    );
    tx.send(Err("creation rejected".into())).unwrap();
    assert_eq!(
        env.view.apply_suggestion_creation(),
        Some(Err("creation rejected".into()))
    );
    assert!(env.view.pending_customized_session_create.is_none());
    assert!(env.view.pending_suggestion_customization.is_some());
    assert!(!env.view.new_dialog.as_ref().unwrap().is_loading());
    let text = render_home_to_string(&mut env.view, 150, 40);
    assert!(text.contains("creation rejected"));
    assert!(env.view.instances().next().is_none());
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.suggestions_view.is_some());
    env.view
        .set_available_tools(AvailableTools::with_tools(&[]));
    env.view.suggestions_view.as_mut().unwrap().prepared =
        Some((prepared.clone(), prepared.source_ref.clone()));
    env.view.tick_dialog();
    assert!(env.view.no_agents_dialog.is_some());
    assert!(env.view.pending_suggestion_customization.is_none());
}

#[test]
#[serial]
fn suggestion_customization_uses_normal_wizard_data_without_changing_security_defaults() {
    let mut env = create_test_env_empty();
    let prepared = prepared(env._temp.path());
    let mut view = SuggestionsView::default();
    view.prepared = Some((prepared.clone(), prepared.source_ref.clone()));
    env.view.suggestions_view = Some(view);
    env.view.tick_dialog();
    let dialog = env.view.new_dialog.as_mut().unwrap();
    dialog.set_title("Reviewed title".into());
    dialog.focus_title();
    let crate::tui::dialogs::DialogResult::Submit(data) = dialog.handle_key(key(KeyCode::Enter))
    else {
        panic!("normal wizard must submit a prepared suggestion");
    };
    assert!(!data.yolo_mode);
    assert!(!data.sandbox);
    assert!(!data.scratch);
    let body = crate::tui::suggestions::SuggestionsClient::create_customized_session_body(
        &data, &prepared,
    )
    .unwrap();
    assert_eq!(body["title"], "Reviewed title");
    assert_eq!(body["prompt"], "Inspect the actual diff");
    assert_eq!(
        body["source_refs"],
        serde_json::json!(["github:repo:pr:42"])
    );
    assert_eq!(
        body["suggestion"],
        serde_json::json!({"provider_id":"github","id":"review:42","revision":7})
    );
    assert_eq!(body["yolo_mode"], false);
    assert_eq!(body["create_another"], false);
    assert!(!body.as_object().unwrap().contains_key("hooks_trusted"));
}
