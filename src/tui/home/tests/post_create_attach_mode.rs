/// Tests for the post-create half of the `default_attach_mode` setting:
/// a freshly-created session enters tmux or live-send mode per the same
/// resolver as Enter/double-click. The unit under test is
/// `HomeView::default_attach_mode` as consumed by the post-create
/// dispatch, plus the invariant that the sync create path emits the
/// routed action variant (so it doesn't bypass the setting the way
/// `Action::AttachSession` would).
use super::*;
use crate::session::config::{update_config, AttachMode};

/// Add a session to the home view, return its id. The instance's
/// `source_profile` is set to "test" so the resolver reads the
/// test profile's config.
fn add_session(view: &mut HomeView, title: &str) -> String {
    let mut inst = Instance::new(title, "/tmp/test");
    inst.source_profile = "test".to_string();
    let id = inst.id.clone();
    view.add_instance(inst);
    id
}

/// Write a global config.toml with the given attach mode so the
/// resolver under test reads the user-configured value. Other
/// fields stay at default.
fn write_global_attach_mode(mode: AttachMode) {
    update_config(|config| {
        config.session.default_attach_mode = mode;
    })
    .unwrap();
}

#[test]
#[serial]
fn defaults_to_tmux_when_no_config_present() {
    // Fresh install: no config.toml exists, no profile override.
    // The setting must resolve to Tmux (historical behavior); a
    // None or LiveSend default would silently change every existing
    // user's UX on upgrade.
    let mut env = create_test_env_empty();
    let id = add_session(&mut env.view, "session-one");
    let mode = env.view.default_attach_mode(&id);
    assert_eq!(
        mode,
        Some(AttachMode::Tmux),
        "default must be Tmux to preserve existing UX"
    );
}

#[test]
#[serial]
fn returns_live_send_when_globally_configured() {
    // User saved `default_attach_mode = "live_send"` in their
    // global config. The resolver must pick it up so the dispatch
    // path in app.rs routes to live mode instead of tmux attach.
    let mut env = create_test_env_empty();
    write_global_attach_mode(AttachMode::LiveSend);
    let id = add_session(&mut env.view, "session-one");
    let mode = env.view.default_attach_mode(&id);
    assert_eq!(mode, Some(AttachMode::LiveSend));
}

#[test]
#[serial]
fn returns_none_for_missing_instance() {
    // Race: the apply_creation_results return reaches the dispatch
    // and the instance has been deleted in the meantime. `None`
    // signals the caller to fall back to the structured view-aware
    // attach_session path rather than try to attach to a ghost.
    let env = create_test_env_empty();
    let mode = env.view.default_attach_mode("nonexistent-id");
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
    write_global_attach_mode(AttachMode::LiveSend);
    let id = add_session(&mut env.view, "acp-one");
    env.view.mutate_instance(&id, |inst| {
        inst.view = crate::session::View::Structured;
    });
    let mode = env.view.default_attach_mode(&id);
    assert!(mode.is_none(), "structured view sessions must return None");
}

/// Build a minimal `NewSessionData` for the sync create path: no
/// sandbox, no hooks (caller passes `None`), no worktree. This is
/// the combination that bypasses `creation_poller` and runs
/// `create_session` inline, which is the path that originally
/// emitted `Action::AttachSession` and bypassed the attach-mode
/// setting.
fn sync_path_session_data(project: &str) -> crate::tui::dialogs::NewSessionData {
    crate::tui::dialogs::NewSessionData {
        profile: "test".to_string(),
        title: "sync-path-test".to_string(),
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
fn sync_create_path_emits_attach_after_create_not_attach_session() {
    // Regression guard for the original bug. `Action::AttachSession`
    // would skip the attach-mode dispatch; only
    // `Action::AttachAfterCreate` routes through it. If a future
    // refactor flips this back, the live-mode setting silently
    // stops working on no-sandbox/no-hooks/no-worktree creates and
    // the bug returns. e2e covers the live-mode end of the
    // dispatch; this unit test covers the action plumbing without
    // needing tmux.
    let mut env = create_test_env_empty();
    let project_dir = env._temp.path().join("sync-project");
    std::fs::create_dir_all(&project_dir).unwrap();
    let data = sync_path_session_data(project_dir.to_str().unwrap());
    let action = env.view.create_session_with_hooks(data, None);
    assert!(
        matches!(action, Some(Action::AttachAfterCreate(_))),
        "sync create path must emit AttachAfterCreate (route through attach-mode setting), got {:?}",
        action
    );
}
