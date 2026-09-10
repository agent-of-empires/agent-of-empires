use super::*;
use crate::daemon::PendingApproval;
use crate::session::Status;
use crate::tui::approval_poller::{ApprovalResolution, ApprovalResult};
use crate::tui::home::PermissionResponseTarget;

fn add_session_with_tool(view: &mut HomeView, title: &str, tool: &str) -> String {
    let mut inst = Instance::new(title, "/tmp/test");
    inst.tool = tool.to_string();
    let id = inst.id.clone();
    view.add_instance(inst);
    id
}
/// Fixture nonce derived at runtime: a literal flowing into a `nonce`
/// field trips CodeQL rust/hard-coded-cryptographic-value (alerts
/// 179-181), test code included.
fn test_nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}
fn pending(nonce: &str) -> Vec<PendingApproval> {
    vec![PendingApproval {
        nonce: nonce.to_string(),
        tool_name: "Bash".to_string(),
        target: "echo hi".to_string(),
        destructive: false,
        choice: false,
    }]
}

#[test]
#[serial]
fn no_selected_session_is_a_no_op() {
    let mut env = create_test_env_empty();
    env.view.selected_session = None;
    let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
    assert!(env.view.permission_response_dialog.is_none());
    assert!(env.view.info_dialog.is_none());
}

#[test]
#[serial]
fn unsupported_agent_shows_info_dialog_no_send() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "some-unmapped-tool");
    env.view.selected_session = Some(id);
    let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
    assert!(
        env.view.permission_response_dialog.is_none(),
        "unsupported agent must not open the dialog"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "unsupported agent must surface an info dialog"
    );
}

#[test]
#[serial]
fn supported_agent_opens_dialog_regardless_of_status() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "claude");
    env.view.selected_session = Some(id.clone());
    // Prove there's no Status::Waiting gate: explicitly set a
    // non-Waiting status before pressing the shortcut.
    env.view
        .mutate_instance(&id, |inst| inst.status = Status::Idle);
    let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
    assert!(
        env.view.permission_response_dialog.is_some(),
        "supported agent + valid target must open the dialog even when not Waiting"
    );
}

#[test]
#[serial]
fn agent_without_allow_always_still_opens_dialog() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "omp");
    env.view.selected_session = Some(id);
    let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
    assert!(
        env.view.permission_response_dialog.is_some(),
        "an agent with allow_always: None must still support allow/deny"
    );
}

#[test]
#[serial]
fn structured_session_reuses_the_permission_dialog() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "claude");
    env.view
        .mutate_instance(&id, |inst| inst.view = crate::session::View::Structured);
    let nonce = test_nonce();
    env.view
        .structured_pending_approvals
        .insert(id.clone(), pending(&nonce));
    env.view.selected_session = Some(id.clone());

    let _ = env.view.handle_key(key(KeyCode::Char('a')), None);

    assert!(
        env.view.permission_response_dialog.is_some(),
        "structured sessions must use the existing permission dialog"
    );
    assert!(matches!(
        env.view.pending_permission_response,
        Some(PermissionResponseTarget::Structured { session_id, nonce: opened })
            if session_id == id && opened == nonce
    ));
}

#[test]
#[serial]
fn choice_list_approval_is_routed_to_the_structured_view() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "claude");
    env.view
        .mutate_instance(&id, |inst| inst.view = crate::session::View::Structured);
    let nonce = test_nonce();
    env.view
        .structured_pending_approvals
        .insert(id.clone(), pending(&nonce));
    // Mark the entry as a choice list (pi's ask_user_question): the generic
    // dialog has no labels to show and its Allow would answer the agent's
    // first option.
    env.view.structured_pending_approvals.get_mut(&id).unwrap()[0].choice = true;
    env.view.selected_session = Some(id.clone());

    let _ = env.view.handle_key(key(KeyCode::Char('a')), None);

    assert!(
        env.view.permission_response_dialog.is_none(),
        "a choice list must not open the generic allow/deny dialog"
    );
    assert!(
        env.view.pending_permission_response.is_none(),
        "no answer may be captured for a choice list outside the labels"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "the user must be directed to the structured view"
    );
}

#[test]
#[serial]
fn resolved_structured_approval_clears_a_nonce_reintroduced_by_polling() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "claude");
    let nonce = test_nonce();
    env.view
        .structured_pending_approvals
        .insert(id.clone(), pending(&nonce));

    env.view.apply_structured_approval_result(ApprovalResult {
        session_id: id.clone(),
        nonce,
        resolution: ApprovalResolution::Resolved,
    });

    assert!(
        !env.view.structured_pending_approvals.contains_key(&id),
        "a completed resolution must clear a nonce reintroduced by polling"
    );
}

#[test]
fn approval_choice_maps_to_the_correct_wire_decision() {
    use crate::acp::protocol::ApprovalDecisionWire;
    use crate::tui::dialogs::PermissionResponseChoice;
    let cases = [
        (PermissionResponseChoice::Allow, ApprovalDecisionWire::Allow),
        (
            PermissionResponseChoice::AllowAlways,
            ApprovalDecisionWire::AllowAlways,
        ),
        (PermissionResponseChoice::Deny, ApprovalDecisionWire::Deny),
    ];
    for (choice, expected) in cases {
        assert_eq!(
            HomeView::approval_decision_wire(choice),
            expected,
            "{choice:?}"
        );
    }
}

#[test]
#[serial]
fn gone_structured_approval_clears_and_informs() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "claude");
    let nonce = test_nonce();
    env.view
        .structured_pending_approvals
        .insert(id.clone(), pending(&nonce));

    env.view.apply_structured_approval_result(ApprovalResult {
        session_id: id.clone(),
        nonce,
        resolution: ApprovalResolution::Gone,
    });

    assert!(
        !env.view.structured_pending_approvals.contains_key(&id),
        "an already-resolved approval must be cleared"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "the user must be told the approval was already answered elsewhere"
    );
}

#[test]
#[serial]
fn failed_structured_approval_informs_and_lets_polling_restore() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "claude");
    // The optimistic removal already ran; the poll has not re-added it yet.
    env.view.apply_structured_approval_result(ApprovalResult {
        session_id: id.clone(),
        nonce: test_nonce(),
        resolution: ApprovalResolution::Failed("boom".to_string()),
    });

    // The failed arm does not re-insert: the still-pending approval is
    // restored by the next daemon poll, not manual bookkeeping.
    assert!(
        !env.view.structured_pending_approvals.contains_key(&id),
        "the failed arm must not manufacture a map entry"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "a resolve failure must surface an error dialog"
    );
}
