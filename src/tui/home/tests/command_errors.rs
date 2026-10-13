//! Unknown outcomes remain recoverable without replaying the lost mutation.

use super::*;
use crate::daemon::{RuntimeCursor, SessionMutation};

#[test]
#[serial]
fn cancelled_or_failed_resolution_keeps_the_real_quarantine_recoverable() {
    let mut env = create_test_env_empty();
    let lost = env.view.session_feed.command_driver_for_test();
    for id in ["a", "b"] {
        env.view
            .session_feed
            .submit(id.into(), SessionMutation::Stop)
            .unwrap();
    }
    drop(lost);
    env.view.apply_restart_results();
    assert_eq!(env.view.pending_indeterminate_queue.len(), 2);
    let first = env.view.pending_indeterminate_queue[0].0.clone();
    let second = env.view.pending_indeterminate_queue[1].0.clone();
    assert!(matches!(
        (first.as_str(), second.as_str()),
        ("a", "b") | ("b", "a")
    ));
    assert!(!env.view.session_feed.can_submit("a"));
    env.view.info_dialog = None;
    env.view
        .handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), None);
    assert!(env.view.pending_indeterminate_resolution.is_none());
    assert_eq!(
        env.view
            .pending_indeterminate_queue
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [first.as_str(), second.as_str()]
    );

    let mut reconnected = env.view.session_feed.command_driver_for_test();
    let mut unapplied = (*env.view.session_feed.applied_snapshot().unwrap()).clone();
    unapplied.cursor.revision = env.view.session_feed.next_revision_for_test();
    let unapplied_cursor = unapplied.cursor.clone();
    env.view
        .session_feed
        .publish_for_test(crate::tui::session_feed::SessionFeedResult::Snapshot(
            std::sync::Arc::new(unapplied),
        ));
    assert!(!env.view.session_feed.receipt_applied(&unapplied_cursor));
    reopen_resolution(&mut env, &first);
    env.view.confirm_dialog = None;
    env.view.dispatch_confirm_submit("resolve_indeterminate");
    assert!(!env.view.session_feed.can_submit(&first));
    assert_eq!(
        env.view
            .pending_indeterminate_queue
            .first()
            .map(|(id, _)| id.as_str()),
        Some(first.as_str())
    );

    env.view.info_dialog = None;
    publish_canonical_rows(&mut env, &[], 2);
    assert!(env.view.instances().next().is_none());
    reopen_resolution(&mut env, &first);
    env.view.confirm_dialog = None;
    env.view.dispatch_confirm_submit("resolve_indeterminate");
    assert!(env.view.session_feed.can_submit(&first));
    assert!(!env.view.session_feed.can_submit(&second));
    assert_eq!(
        env.view.pending_indeterminate_resolution.as_deref(),
        Some(second.as_str())
    );
    assert_eq!(
        env.view
            .pending_indeterminate_queue
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        [second.as_str()]
    );
    assert!(
        reconnected(Ok(RuntimeCursor {
            epoch: "test".into(),
            revision: 3
        }))
        .is_none(),
        "resolution must not replay the lost mutation"
    );
    env.view
        .session_feed
        .submit(first.clone(), SessionMutation::Stop)
        .unwrap();
    assert_eq!(
        reconnected(Ok(RuntimeCursor {
            epoch: "test".into(),
            revision: 3
        }))
        .map(|(id, _)| id),
        Some(first)
    );
}

fn reopen_resolution(env: &mut TestEnv, expected: &str) {
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        None,
    );
    for key in "Resolve unknown runtime change".chars() {
        env.view
            .handle_key(KeyEvent::new(KeyCode::Char(key), KeyModifiers::NONE), None);
    }
    env.view
        .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), None);
    assert_eq!(
        env.view.pending_indeterminate_resolution.as_deref(),
        Some(expected)
    );
}
