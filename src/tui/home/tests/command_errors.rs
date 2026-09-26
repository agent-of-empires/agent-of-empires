//! Command-error presentation: a drain is destructive, so everything it
//! removes must reach the user.

use super::*;
use crate::tui::session_feed::SessionCommandError;

fn error(id: &str, message: &str, unknown: bool) -> SessionCommandError {
    SessionCommandError {
        id: id.into(),
        message: message.into(),
        marks_unread: false,
        outcome_unknown: unknown,
    }
}

fn info_text(env: &TestEnv) -> String {
    env.view
        .info_dialog
        .as_ref()
        .map(|d| d.message().to_string())
        .unwrap_or_default()
}

#[test]
#[serial]
fn every_drained_diagnostic_reaches_the_user() {
    // A batch must not be summarized down to its first entry: the other
    // messages are the only record of what also failed.
    let mut env = create_test_env_empty();
    env.view
        .session_feed
        .queue_error_for_test(error("a", "first failure", false));
    env.view
        .session_feed
        .queue_error_for_test(error("b", "second failure", false));

    assert!(env.view.apply_restart_results(), "an error was drained");
    let text = info_text(&env);
    assert!(text.contains("first failure"), "got: {text}");
    assert!(text.contains("second failure"), "got: {text}");
}

#[test]
#[serial]
fn apply_restart_results_surfaces_a_drained_error() {
    // The restart settle path drains the same buffer; discarding its result
    // would be the one way a failed restart leaves no trace at all.
    let mut env = create_test_env_empty();
    env.view
        .session_feed
        .queue_error_for_test(error("a", "restart rejected", false));

    assert!(env.view.apply_restart_results());
    assert!(
        info_text(&env).contains("restart rejected"),
        "the restart path must present what it drained"
    );
}

#[test]
#[serial]
fn a_batch_of_unknown_outcomes_keeps_every_id() {
    // Each unknown row stays quarantined until the user resolves it, so
    // keeping only the first would strand the rest with no unlock prompt.
    let mut env = create_test_env_empty();
    env.view
        .session_feed
        .queue_error_for_test(error("a", "outcome a unknown", true));
    env.view
        .session_feed
        .queue_error_for_test(error("b", "outcome b unknown", true));

    assert!(env.view.apply_restart_results());
    let queued: Vec<&str> = env
        .view
        .pending_indeterminate_queue
        .iter()
        .map(|(id, _)| id.as_str())
        .collect();
    assert_eq!(queued, ["a", "b"], "every unknown id must be retained");
    assert_eq!(
        env.view.pending_indeterminate_resolution.as_deref(),
        Some("a"),
        "the first unknown id drives the dialog"
    );
}

#[test]
#[serial]
fn resolving_one_unknown_promotes_the_next() {
    let mut env = create_test_env_empty();
    env.view
        .session_feed
        .queue_error_for_test(error("a", "outcome a unknown", true));
    env.view
        .session_feed
        .queue_error_for_test(error("b", "outcome b unknown", true));
    env.view.apply_restart_results();
    env.view.confirm_dialog = None;

    env.view.dispatch_confirm_submit("resolve_indeterminate");

    assert_eq!(
        env.view.pending_indeterminate_resolution.as_deref(),
        Some("b")
    );
    assert_eq!(
        env.view
            .pending_indeterminate_queue
            .first()
            .map(|(id, _)| id.as_str()),
        Some("b"),
        "the promoted id stays queued until its own resolution"
    );
}

#[test]
#[serial]
fn an_empty_drain_presents_nothing() {
    let mut env = create_test_env_empty();
    assert!(!env.view.apply_restart_results());
    assert!(env.view.info_dialog.is_none());
    assert!(env.view.confirm_dialog.is_none());
}
