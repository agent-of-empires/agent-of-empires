use super::*;
use crate::session::Status;

fn add_session_with_tool(view: &mut HomeView, title: &str, tool: &str) -> String {
    let mut inst = Instance::new(title, "/tmp/test");
    inst.tool = tool.to_string();
    let id = inst.id.clone();
    view.add_instance(inst);
    id
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
fn structured_session_is_a_no_op() {
    let mut env = create_test_env_empty();
    let id = add_session_with_tool(&mut env.view, "session-one", "claude");
    env.view
        .mutate_instance(&id, |inst| inst.view = crate::session::View::Structured);
    env.view.selected_session = Some(id);
    let _ = env.view.handle_key(key(KeyCode::Char('a')), None);
    assert!(
        env.view.permission_response_dialog.is_none(),
        "structured (ACP) session must not open the tmux-keystroke dialog"
    );
    assert!(
        env.view.info_dialog.is_none(),
        "structured session no-op must be silent, not surface an info dialog"
    );
}
