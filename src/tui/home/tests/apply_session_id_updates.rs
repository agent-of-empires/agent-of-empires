use super::*;

#[tokio::test]
#[serial]
async fn canonical_identity_updates_never_install_a_tui_poller_or_write_payloads() {
    let (_temp, _guard) = test_home();
    let mut row = Instance::new("identity", "/tmp/identity");
    row.source_profile = "test".into();
    row.tool = "claude".into();
    row.status = Status::Idle;
    seed_profile("test", std::slice::from_ref(&row));
    let state = native_state(&["test"]).await;
    let mut view = test_view(Some("test"));
    apply_published(&mut view, &state).await;
    {
        let mut rows = state.instances.write().await;
        crate::server::test_support::attach_session_id_update_for_test(
            &mut rows[0],
            "019342ab-1111-7aaa-8bbb-cccdddeeefff",
        );
    }
    crate::server::test_support::drain_session_id_updates_for_test(&state).await;
    let stored = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert_eq!(
        stored[0].agent_session_id.as_deref(),
        Some("019342ab-1111-7aaa-8bbb-cccdddeeefff")
    );
    let bytes = payload_bytes("test");
    apply_published(&mut view, &state).await;
    assert_eq!(
        view.get_instance(&row.id).unwrap().agent_session_id,
        stored[0].agent_session_id
    );
    assert!(view
        .get_instance(&row.id)
        .unwrap()
        .capture_poller
        .handle()
        .is_none());
    view.reload().unwrap();
    assert_eq!(payload_bytes("test"), bytes);
}

/// Runtime reads resolve to the main repo, so Settings has to edit that copy.
#[test]
#[serial]
fn settings_repo_scope_saves_where_a_workspace_runtime_reads() {
    let tmp = tempfile::tempdir().unwrap();
    let main_repo = tmp.path().join("repo");
    let git = git2::Repository::init(&main_repo).unwrap();
    let signature = git2::Signature::now("t", "t@example.com").unwrap();
    let tree = git
        .find_tree(git.index().unwrap().write_tree().unwrap())
        .unwrap();
    git.commit(Some("HEAD"), &signature, &signature, "init", &tree, &[])
        .unwrap();
    let workspace = tmp.path().join("workspace");
    let checkout = workspace.join("repo");
    std::fs::create_dir_all(&workspace).unwrap();
    git.worktree("feat", &checkout, None).unwrap();

    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instances().next().unwrap().id.clone();
    env.view.selected_session = Some(id.clone());
    let workspace_dir = workspace.to_string_lossy().into_owned();
    let repo_path = main_repo.to_string_lossy().into_owned();
    let worktree_path = checkout.to_string_lossy().into_owned();
    env.view.mutate_instance(&id, |inst| {
        inst.project_path = workspace_dir.clone();
        inst.workspace_info = Some(crate::session::WorkspaceInfo {
            branch: "feat".into(),
            workspace_dir,
            created_at: chrono::Utc::now(),
            cleanup_on_delete: false,
            repos: vec![crate::session::WorkspaceRepo {
                name: "repo".into(),
                source_path: repo_path.clone(),
                branch: "feat".into(),
                worktree_path,
                main_repo_path: repo_path,
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            }],
        });
    });

    use crate::session::config::repo_config::{
        resolve_config_with_repo, save_repo_config, RepoConfig,
    };
    let parse = |text: &str| toml::from_str::<RepoConfig>(text).unwrap();
    save_repo_config(&main_repo, &parse("[worktree]\nauto_cleanup = true\n")).unwrap();

    env.view.open_settings();

    let settings = env.view.settings_view.as_mut().unwrap();
    assert!(
        settings.repo_config.is_some(),
        "the form loads the main repo's values"
    );
    settings.scope = crate::tui::settings::SettingsScope::Repo;
    settings.repo_config = Some(parse("[worktree]\nauto_cleanup = false\n"));
    settings.save().unwrap();

    let profile = env.view.config_profile();
    assert!(
        !resolve_config_with_repo(&profile, &checkout)
            .unwrap()
            .worktree
            .auto_cleanup,
        "runtime resolution sees the saved value"
    );

    let plain_worktree = checkout.to_string_lossy().into_owned();
    env.view.mutate_instance(&id, |inst| {
        inst.workspace_info = None;
        inst.project_path = plain_worktree.clone();
    });
    env.view.open_settings();
    assert_eq!(
        env.view.settings_view.as_ref().unwrap().project_path,
        Some(plain_worktree)
    );
}

/// Discarding unsaved Settings changes via a mouse click on the
/// confirmation dialog's [Yes] button must revert a live theme preview,
/// exactly like the keyboard discard path. Regression for the
/// empire -> rose-pine flip where the click path closed Settings but
/// never dispatched `SetTheme`, leaving the previewed theme applied until
/// the next restart.
#[test]
#[serial]
fn settings_mouse_discard_reverts_theme_preview() {
    use crate::tui::dialogs::ConfirmDialog;
    use crate::tui::styles::load_theme;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    let mut env = create_test_env_empty();
    let view = &mut env.view;
    view.open_settings();
    assert!(view.settings_view.is_some(), "settings view should open");

    // Stand in the state reached after the user previewed a theme (so the
    // view has unsaved changes) and pressed Esc to close: the unsaved-
    // changes confirm dialog floats over the settings takeover.
    view.settings_close_confirm = true;
    view.confirm_dialog = Some(ConfirmDialog::new(
        "Unsaved Changes",
        "You have unsaved changes. Discard them?",
        "discard_settings",
    ));

    // Render once so the dialog's [Yes] button hit-rect is populated at the
    // exact coordinates it draws.
    let theme = load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            view.render(f, area, &theme, None, None, None);
        })
        .unwrap();

    let yes = view
        .confirm_dialog
        .as_ref()
        .unwrap()
        .yes_button_area_for_test();
    assert!(yes.width > 0, "render should populate the [Yes] hit-rect");

    // Click the center of [Yes] to discard.
    view.handle_dialog_click(yes.x + yes.width / 2, yes.y + yes.height / 2);

    // The click path must queue the same theme revert the keyboard path
    // returns. Before the fix this was `None` and the previewed theme stuck.
    assert!(
        matches!(view.pending_dialog_click_action, Some(Action::SetTheme(_))),
        "mouse discard should queue a SetTheme revert, got {:?}",
        view.pending_dialog_click_action
    );
    assert!(view.settings_view.is_none(), "settings should be closed");
    assert!(!view.settings_close_confirm, "confirm flag should reset");
}
