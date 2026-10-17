/// Tests for the mode that opens a newly created terminal-mode session: the default follows
/// `default_attach_mode`, and an explicit mode applies only after creation.
use super::*;
use crate::session::config::{update_config, AttachMode, NewSessionMode};

fn add_session(view: &mut HomeView, title: &str) -> String {
    let mut inst = Instance::new(title, "/tmp/test");
    inst.source_profile = "test".to_string();
    let id = inst.id.clone();
    view.add_instance(inst);
    id
}

fn write_session_modes(default_attach_mode: AttachMode, new_session_mode: NewSessionMode) {
    update_config(|config| {
        config.session.default_attach_mode = default_attach_mode;
        config.session.new_session_mode = new_session_mode;
    })
    .unwrap();
}

#[test]
#[serial]
fn resolves_new_session_mode() {
    let mut env = create_test_env_empty();
    let cases = [
        (
            AttachMode::Tmux,
            NewSessionMode::MatchDefault,
            AttachMode::Tmux,
        ),
        (
            AttachMode::LiveSend,
            NewSessionMode::MatchDefault,
            AttachMode::LiveSend,
        ),
        (
            AttachMode::Tmux,
            NewSessionMode::LiveSend,
            AttachMode::LiveSend,
        ),
        (AttachMode::LiveSend, NewSessionMode::Tmux, AttachMode::Tmux),
    ];
    for (default_attach_mode, new_session_mode, expected) in cases {
        write_session_modes(default_attach_mode, new_session_mode);
        let id = add_session(&mut env.view, "session-one");
        assert_eq!(
            env.view.new_session_attach_mode(&id),
            Some(expected),
            "{new_session_mode:?} with {default_attach_mode:?}"
        );
    }

    // None sends the dispatch to the structured-aware attach fallback: the instance was
    // deleted before the creation result landed, or it is a structured session with no
    // tmux target.
    assert!(env.view.new_session_attach_mode("nonexistent-id").is_none());
    write_session_modes(AttachMode::LiveSend, NewSessionMode::MatchDefault);
    let id = add_session(&mut env.view, "acp-one");
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Structured;
    });
    assert!(
        env.view.new_session_attach_mode(&id).is_none(),
        "structured view sessions must return None"
    );
}

fn plain_session_data(project: &str) -> crate::tui::dialogs::NewSessionData {
    crate::tui::dialogs::NewSessionData {
        profile: "test".to_string(),
        title: "plain-create-test".to_string(),
        title_typed: false,
        path: project.to_string(),
        group: String::new(),
        tool: "claude".to_string(),
        worktree_enabled: false,
        worktree_branch: None,
        create_new_branch: false,
        base_branch: None,
        extra_repo_paths: Vec::new(),
        sandbox: false,
        sandbox_image: String::new(),
        yolo_mode: false,
        extra_env: Vec::new(),
        extra_args: String::new(),
        command_override: String::new(),
        scratch: false,
        fork_seed: None,
        structured: false,
    }
}

#[test]
#[serial]
fn plain_creation_completes_through_configured_attach_mode() {
    let mut env = create_test_env_empty();
    let project_dir = env._temp.path().join("plain-project");
    std::fs::create_dir_all(&project_dir).unwrap();
    for (default, creation, expected) in [
        (
            AttachMode::Tmux,
            NewSessionMode::LiveSend,
            AttachMode::LiveSend,
        ),
        (AttachMode::LiveSend, NewSessionMode::Tmux, AttachMode::Tmux),
    ] {
        write_session_modes(default, creation);
        let mut data = plain_session_data(project_dir.to_str().unwrap());
        data.title = format!("{expected:?}");
        assert_eq!(env.view.create_session_with_hooks(data, None), None);
        let id = drain_creation_result(&mut env.view).expect("plain creation committed");
        assert_eq!(env.view.new_session_attach_mode(&id), Some(expected));
        assert_ne!(env.view.get_instance(&id).unwrap().status, Status::Creating);
        assert_eq!(
            env.view.apply_creation_results(),
            None,
            "attach is delivered once"
        );
    }
}
