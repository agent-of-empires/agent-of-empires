use super::*;
use crate::daemon::PendingApproval;
use crate::session::Status;
use crate::tui::approval_poller::{ApprovalResolution, ApprovalResult};
use crate::tui::home::PermissionResponseTarget;
use aoe_plugin_api::agent_integration::{
    AgentIntegrationOption, AgentIntegrationPrompt, AgentIntegrationQuestion,
    AgentIntegrationSnapshot, AgentIntegrationStatus,
};

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

/// 'a' opens the permission dialog for any mapped agent, with no Waiting gate and even
/// without allow-always (omp); an unmapped agent gets an info dialog and nothing is sent,
/// and no selection is a no-op.
#[test]
#[serial]
fn a_opens_permission_dialog_only_for_supported_agents() {
    let cases = [
        (None, false, false),
        (Some("some-unmapped-tool"), false, true),
        (Some("claude"), true, false),
        (Some("omp"), true, false),
    ];
    for (tool, dialog, info) in cases {
        let mut env = create_test_env_empty();
        let selected = tool.map(|tool| {
            let id = add_session_with_tool(&mut env.view, "session-one", tool);
            env.view
                .mutate_instance(&id, |inst| inst.status = Status::Idle);
            id
        });
        env.view.selected_session = selected;
        let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
        assert_eq!(
            env.view.permission_response_dialog.is_some(),
            dialog,
            "{tool:?}"
        );
        assert_eq!(env.view.info_dialog.is_some(), info, "{tool:?}");
    }
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

fn integration_snapshot(
    instance_id: &str,
    prompt: AgentIntegrationPrompt,
) -> AgentIntegrationSnapshot {
    AgentIntegrationSnapshot {
        schema_version: 1,
        instance_id: instance_id.to_string(),
        provider_version: "1.0.0".to_string(),
        provider_session_id: "provider-session".to_string(),
        writer_pid: std::process::id(),
        revision: 1,
        status: AgentIntegrationStatus::Waiting,
        reply_keys: vec!["F12".to_string()],
        prompt: Some(prompt),
    }
}

#[test]
#[serial]
fn removed_integration_session_reports_response_not_sent() {
    use crate::tui::dialogs::{AgentQuestionDialog, PermissionResponseDialog};

    let prompts = [
        AgentIntegrationPrompt::Approval {
            id: "approval".to_string(),
            title: "Run command?".to_string(),
            allow_always: false,
        },
        AgentIntegrationPrompt::Question {
            id: "question".to_string(),
            title: "Choose a database".to_string(),
            questions: vec![AgentIntegrationQuestion {
                id: "database".to_string(),
                question: "Which database?".to_string(),
                options: vec![AgentIntegrationOption {
                    label: "SQLite".to_string(),
                    description: None,
                }],
                multi: false,
                recommended: None,
            }],
        },
    ];

    for prompt in prompts {
        let mut env = create_test_env_empty();
        let id = add_session_with_tool(&mut env.view, "session-one", "claude");
        let snapshot = integration_snapshot(&id, prompt.clone());
        env.view.pending_permission_response = Some(PermissionResponseTarget::Integration {
            session_id: id.clone(),
            snapshot,
        });

        let submit_key =
            match &prompt {
                AgentIntegrationPrompt::Approval {
                    title,
                    allow_always,
                    ..
                } => {
                    env.view.permission_response_dialog = Some(
                        PermissionResponseDialog::integration("session-one", title, *allow_always),
                    );
                    KeyCode::Char('a')
                }
                AgentIntegrationPrompt::Question { .. } => {
                    env.view.agent_question_dialog = Some(AgentQuestionDialog::new(prompt.clone()));
                    KeyCode::Enter
                }
            };

        env.view.remove_instance(&id);
        assert!(env.view.get_instance(&id).is_none());
        let _ = env.view.handle_key(key(submit_key), None);

        let info = env.view.info_dialog.as_ref().expect("response warning");
        assert_eq!(info.title(), "Response Not Sent");
        assert_eq!(
            info.message(),
            "The session is no longer available. The response was not sent."
        );
    }
}
