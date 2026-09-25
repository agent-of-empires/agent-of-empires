/// Tests for the mode that opens a newly-created terminal-mode session. The
/// default follows `default_attach_mode`, preserving historical behavior. An
/// explicit mode applies only after creation.
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
}

#[test]
#[serial]
fn returns_none_for_missing_instance() {
    // Race: the apply_creation_results return reaches the dispatch
    // and the instance has been deleted in the meantime. `None`
    // signals the caller to fall back to the structured view-aware
    // attach_session path rather than try to attach to a ghost.
    let env = create_test_env_empty();
    let mode = env.view.new_session_attach_mode("nonexistent-id");
    assert!(mode.is_none());
}

#[test]
#[serial]
fn returns_none_for_acp_session() {
    // Acp sessions aren't tmux-backed; live mode has no target
    // and tmux attach is a no-op. The resolver returns None so the
    // dispatch picks the (no-op) fallback explicitly, regardless of
    // what the user configured globally.
    let mut env = create_test_env_empty();
    write_session_modes(AttachMode::LiveSend, NewSessionMode::MatchDefault);
    let id = add_session(&mut env.view, "acp-one");
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Structured;
    });
    let mode = env.view.new_session_attach_mode(&id);
    assert!(mode.is_none(), "structured view sessions must return None");
}
