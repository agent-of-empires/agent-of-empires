//! List rendering and persisted view state.

use super::*;

fn worktree_instance(title: &str) -> Instance {
    let mut inst = Instance::new(title, "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    inst
}

fn workspace_instance() -> Instance {
    let repo = |name: &str| crate::session::WorkspaceRepo {
        name: name.to_string(),
        source_path: format!("/src/{name}"),
        branch: "feature/foo".to_string(),
        worktree_path: format!("/tmp/workspace/{name}"),
        main_repo_path: format!("/src/{name}"),
        managed_by_aoe: true,
        branch_preexisting: false,
        base_branch: None,
        base_branch_override: None,
    };
    let mut inst = Instance::new("workspace-session", "/tmp/workspace");
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/foo".to_string(),
        workspace_dir: "/tmp/workspace".to_string(),
        repos: vec![repo("api"), repo("web")],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });
    inst
}

/// Row tags in the all-profiles view. Branch (the default) owns the branch suffix: a compact
/// last-segment tag, never the raw branch, whether or not the title matches it, and
/// `branch+N` for workspaces. None hides every suffix; Auto shows the profile short code.
#[test]
#[serial]
fn test_row_tag_modes_in_all_profiles_view() {
    use crate::session::config::RowTagMode;
    let alpha_tag = crate::tui::home::render::RowTag {
        content: crate::tui::home::render::profile_short_code("alpha"),
        max_width: 4,
    }
    .rendered();
    let alpha_needles = [alpha_tag.as_str()];
    let cases: [(Instance, RowTagMode, &[&str], &[&str]); 7] = [
        (
            worktree_instance("my-session"),
            RowTagMode::default(),
            &["[foo         ]"],
            &[],
        ),
        (
            worktree_instance("my-session"),
            RowTagMode::Branch,
            &["[foo         ]"],
            &["feature/foo"],
        ),
        (
            worktree_instance("feature/foo"),
            RowTagMode::Branch,
            &["[foo         ]"],
            &[],
        ),
        (
            worktree_instance("my-session"),
            RowTagMode::None,
            &[],
            &["feature/foo", "[foo", "["],
        ),
        (
            workspace_instance(),
            RowTagMode::None,
            &[],
            &["feature/foo", "repos", "["],
        ),
        (
            workspace_instance(),
            RowTagMode::Branch,
            &["[foo+2       ]"],
            &[],
        ),
        (
            Instance::new("A1", "/tmp/a"),
            RowTagMode::Auto,
            &alpha_needles,
            &[],
        ),
    ];
    for (inst, mode, present, absent) in cases {
        let text = rendered_single_session_text(inst, mode);
        for needle in present {
            assert!(
                text.contains(*needle),
                "{mode:?}: missing {needle:?} in {text:?}"
            );
        }
        for needle in absent {
            assert!(
                !text.contains(*needle),
                "{mode:?}: unexpected {needle:?} in {text:?}"
            );
        }
    }
}

/// In a single-profile view Auto omits the profile tag (the list title already names it);
/// Profile renders it anyway.
#[test]
#[serial]
fn test_row_tag_profile_modes_in_filtered_view() {
    use crate::session::config::RowTagMode;
    let (_temp, _guard) = test_home();
    seed_profile("alpha", &[Instance::new("A1", "/tmp/a")]);
    let mut view = test_view(Some("alpha"));
    view.group_by = crate::session::config::GroupByMode::Manual;
    let rendered = crate::tui::home::render::RowTag {
        content: crate::tui::home::render::profile_short_code("alpha"),
        max_width: 4,
    }
    .rendered();
    for (mode, expect_tag) in [(RowTagMode::Auto, false), (RowTagMode::Profile, true)] {
        view.row_tag_mode = mode;
        view.flat_items = view.build_flat_items();
        let row = view
            .flat_items
            .iter()
            .find(|item| matches!(item, Item::Session { .. }))
            .expect("session row");
        let text = rendered_row_text(&view, row);
        assert_eq!(text.contains(&rendered), expect_tag, "{mode:?}: {text:?}");
    }
}

#[test]
#[serial]
fn test_create_session_in_all_mode_is_findable() {
    use crate::tui::dialogs::NewSessionData;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Create a profile so "all" mode has something
    let storage = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("Existing", "/tmp/a")];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let project_dir = temp.path().join("project");
    std::fs::create_dir_all(&project_dir).unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let data = NewSessionData {
        profile: "alpha".to_string(),
        title: "New Session".to_string(),
        path: project_dir.to_str().unwrap().to_string(),
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
    };

    let session_id = view.create_session(data).unwrap();

    // In unified view, the session IS findable (fixes #419)
    assert!(
        view.get_instance(&session_id).is_some(),
        "session created in all-mode should be findable by get_instance"
    );
    assert_eq!(
        view.get_instance(&session_id).unwrap().source_profile,
        "alpha"
    );
}

#[test]
#[serial]
fn test_save_preserves_per_profile_collapsed_state() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Create alpha with group "work" (collapsed)
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut inst_a = Instance::new("A1", "/tmp/a");
    inst_a.group_path = "work".to_string();
    let mut tree_a = GroupTree::new_with_groups(&[inst_a.clone()], &[]);
    tree_a.toggle_collapsed("work");
    storage_a
        .update(|i, g| {
            *i = [inst_a].to_vec();
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    // Create beta with group "work" (expanded, the default)
    let storage_b = Storage::new_unwatched("beta").unwrap();
    let mut inst_b = Instance::new("B1", "/tmp/b");
    inst_b.group_path = "work".to_string();
    let tree_b = GroupTree::new_with_groups(&[inst_b.clone()], &[]);
    storage_b
        .update(|i, g| {
            *i = [inst_b].to_vec();
            *g = tree_b.get_all_groups();
            Ok(())
        })
        .unwrap();

    // Load unified view
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Verify per-profile collapsed state is preserved
    let alpha_tree = view.group_trees.get("alpha").unwrap();
    let alpha_work = alpha_tree
        .get_all_groups()
        .into_iter()
        .find(|g| g.path == "work")
        .expect("alpha should have work group");
    assert!(
        alpha_work.collapsed,
        "alpha's 'work' group should be collapsed"
    );

    let beta_tree = view.group_trees.get("beta").unwrap();
    let beta_work = beta_tree
        .get_all_groups()
        .into_iter()
        .find(|g| g.path == "work")
        .expect("beta should have work group");
    assert!(
        !beta_work.collapsed,
        "beta's 'work' group should be expanded"
    );

    // Save and reload to verify persistence
    view.save().unwrap();

    // Reload from disk and verify alpha's collapsed state survived
    let (_, groups_a) = storage_a.load_with_groups().unwrap();
    let saved_a = groups_a
        .iter()
        .find(|g| g.path == "work")
        .expect("alpha should still have work group on disk");
    assert!(
        saved_a.collapsed,
        "alpha's 'work' collapsed state should persist to disk"
    );

    let (_, groups_b) = storage_b.load_with_groups().unwrap();
    let saved_b = groups_b
        .iter()
        .find(|g| g.path == "work")
        .expect("beta should still have work group on disk");
    assert!(
        !saved_b.collapsed,
        "beta's 'work' expanded state should persist to disk"
    );
}

/// Group delete is scoped to the selected group's profile when several profiles own a
/// same-named group: an empty one opens the simple confirm rather than the "delete N
/// sessions" dialog driven by a populated twin, and deleting a populated one leaves the
/// other profiles' group and members alone.
#[test]
#[serial]
fn test_group_delete_scoped_to_owning_profile() {
    let (_temp, _guard) = test_home();
    seed_profile("alpha", &[instance_in("A1", "/tmp/a", "work")]);
    seed_profile("beta", &[instance_in("B1", "/tmp/b", "work")]);
    Storage::new_unwatched("gamma")
        .unwrap()
        .update(|_instances, groups| {
            groups.push(Group::new("work", "work"));
            Ok(())
        })
        .unwrap();
    let mut view = test_view(None);
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let select_work = |view: &mut HomeView, profile: &str| {
        let work_indices: Vec<usize> = view
            .flat_items
            .iter()
            .enumerate()
            .filter_map(|(idx, item)| match item {
                Item::Group { path, .. } if path == "work" => Some(idx),
                _ => None,
            })
            .collect();
        for idx in work_indices {
            view.cursor = idx;
            view.update_selected();
            if view.selected_group_profile.as_deref() == Some(profile) {
                break;
            }
        }
        assert_eq!(view.selected_group.as_deref(), Some("work"));
        assert_eq!(view.selected_group_profile.as_deref(), Some(profile));
    };

    select_work(&mut view, "gamma");
    view.open_delete_for_selected();
    assert!(
        view.group_delete_options_dialog.is_none(),
        "empty group must not trigger the with-sessions options dialog from a same-named group in another profile"
    );
    assert!(
        view.confirm_dialog.is_some(),
        "empty group should open the simple delete-group confirm"
    );
    view.confirm_dialog = None;

    select_work(&mut view, "alpha");
    view.delete_selected_group().unwrap();
    assert!(
        !view.group_trees.get("alpha").unwrap().group_exists("work"),
        "alpha's 'work' group should be deleted"
    );
    assert!(
        view.group_trees.get("beta").unwrap().group_exists("work"),
        "beta's 'work' group should be untouched"
    );
    let group_of = |profile: &str| {
        view.instances()
            .find(|i| i.source_profile == profile)
            .unwrap()
            .group_path
            .clone()
    };
    assert_eq!(
        group_of("alpha"),
        "",
        "alpha's instance should be ungrouped"
    );
    assert_eq!(
        group_of("beta"),
        "work",
        "beta's instance should stay in 'work'"
    );
}

// Four rename-collision behaviors (untied duplicate-pair reject, group-only change allowed,
// tied derived-destination collision, cross-profile target collision) share one test because
// they need the same `#[serial]`-forcing setup: an isolated home, several HomeView/Storage
// instances, and a process-global `tie_workdir_to_name` flip. Each asserts independently.
#[test]
#[serial]
fn test_rename_selected_rejects_all_identity_collisions_and_allows_group_only_change() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let existing = Instance::new("main branch", "/tmp/repo/");
    let target = Instance::new("throwaway", "/tmp/stale");
    let target_id = target.id.clone();
    storage
        .update(|instances, _groups| {
            *instances = vec![existing, target];
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new_for_test(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.selected_session = Some(target_id.clone());
    storage
        .update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|instance| instance.id == target_id)
                .unwrap()
                .project_path = "/tmp/repo".to_string();
            Ok(())
        })
        .unwrap();

    view.rename_selected("main branch", None, None, false)
        .unwrap();
    assert!(view.info_dialog.is_some());
    assert_eq!(view.get_instance(&target_id).unwrap().title, "throwaway");
    assert_eq!(
        view.get_instance(&target_id).unwrap().project_path,
        "/tmp/repo"
    );

    view.info_dialog = None;
    view.rename_selected("", Some("work"), None, false).unwrap();
    assert!(view.info_dialog.is_none());
    assert_eq!(view.get_instance(&target_id).unwrap().group_path, "work");
    let stored = storage.load().unwrap();
    let target = stored
        .iter()
        .find(|instance| instance.id == target_id)
        .unwrap();
    assert_eq!(target.title, "throwaway");
    assert_eq!(target.group_path, "work");

    // Tied routing derives the destination path from the new title. Reject a
    // collision on that final pair before attempting the git worktree move.
    let tie_guard = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let derived_existing = Instance::new("main branch", "/tmp/worktrees/main-branch");
    let mut tied_target = Instance::new("throwaway", "/tmp/worktrees/throwaway");
    tied_target.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "throwaway".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let tied_id = tied_target.id.clone();
    storage
        .update(|instances, _groups| {
            *instances = vec![derived_existing, tied_target];
            Ok(())
        })
        .unwrap();
    view.reload().unwrap();
    view.selected_session = Some(tied_id.clone());
    view.info_dialog = None;
    view.rename_selected("main branch", None, None, false)
        .unwrap();
    assert!(
        view.info_dialog.is_some(),
        "tied derived-destination collision must be rejected"
    );
    let tied_stored = storage.load().unwrap();
    let tied_target = tied_stored
        .iter()
        .find(|instance| instance.id == tied_id)
        .unwrap();
    assert_eq!(tied_target.title, "throwaway");
    assert_eq!(tied_target.project_path, "/tmp/worktrees/throwaway");

    drop(tie_guard);

    // Moving between profiles checks the authoritative target storage, not
    // only the source profile or unified-view cache.
    let alpha = Storage::new_unwatched("alpha").unwrap();
    let beta = Storage::new_unwatched("beta").unwrap();
    let source = Instance::new("source", "/tmp/profile-collision");
    let source_id = source.id.clone();
    alpha
        .update(|instances, _groups| {
            *instances = vec![source];
            Ok(())
        })
        .unwrap();
    beta.update(|instances, _groups| {
        *instances = vec![Instance::new("occupied", "/tmp/profile-collision")];
        Ok(())
    })
    .unwrap();
    let mut unified = HomeView::new_for_test(
        None,
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    unified.selected_session = Some(source_id.clone());
    let error = unified
        .rename_selected("occupied", None, Some("beta"), false)
        .expect_err("target-profile identity collision must reject the transaction");
    assert!(
        error
            .to_string()
            .contains("Session already exists with same title and path"),
        "unexpected collision error: {error:#}"
    );
    assert_eq!(
        alpha
            .load()
            .unwrap()
            .iter()
            .find(|instance| instance.id == source_id)
            .unwrap()
            .title,
        "source"
    );
    assert_eq!(beta.load().unwrap().len(), 1);
}

/// Changing a session's profile via the rename dialog must transfer its group metadata in
/// the same storage transaction, or the source reloads an empty duplicate while the target
/// row renders under a separately created group.
#[test]
#[serial]
fn test_rename_profile_change_prunes_source_group() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // alpha has one session in "work"; beta exists but is empty.
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut inst_a = Instance::new("A1", "/tmp/a");
    inst_a.group_path = "work".to_string();
    let id = inst_a.id.clone();
    let tree_a = GroupTree::new_with_groups(&[inst_a.clone()], &[]);
    storage_a
        .update(|i, g| {
            *i = [inst_a].to_vec();
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();
    let _storage_b = Storage::new_unwatched("beta").unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.selected_session = Some(id.clone());

    // Move the session alpha -> beta, keeping the same group name.
    view.rename_selected("", None, Some("beta"), false).unwrap();

    let moved = view.get_instance(&id).unwrap();
    assert_eq!(moved.source_profile, "beta");
    assert_eq!(moved.group_path, "work");
    assert!(
        view.group_trees.get("beta").unwrap().group_exists("work"),
        "beta should own the 'work' group after the move"
    );
    assert!(
        !view
            .group_trees
            .get("alpha")
            .map(|t| t.group_exists("work"))
            .unwrap_or(false),
        "alpha's now-empty 'work' group should be pruned after the profile move"
    );
    let (_, source_groups) = Storage::new_unwatched("alpha")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let (_, target_groups) = Storage::new_unwatched("beta")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert!(!source_groups.iter().any(|group| group.path == "work"));
    assert!(target_groups.iter().any(|group| group.path == "work"));
}

#[test]
#[serial]
fn test_shift_n_opens_prefilled_dialog_from_session() {
    let mut env = create_test_env_with_groups();
    assert!(env.view.new_dialog.is_none());

    // Move cursor to the "work-project" session (grouped under "work")
    // flat_items: [Group("personal"), Session("personal-project"), Group("work"), Session("work-project"), Session("ungrouped")]
    let work_session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if env.view.get_instance(id).map(|i| i.title.as_str()) == Some("work-project")))
        .expect("work-project session should exist in flat_items");
    env.view.cursor = work_session_idx;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('N')), None);
    let dialog = env.view.new_dialog.as_ref().expect("N should open dialog");
    assert_eq!(dialog.path_value(), "/tmp/work");
    assert_eq!(dialog.group_value(), "work");
}

#[test]
#[serial]
fn test_shift_n_opens_prefilled_dialog_from_group() {
    let mut env = create_test_env_with_groups();

    // Move cursor to a group row
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == "work"))
        .expect("work group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('N')), None);
    let dialog = env.view.new_dialog.as_ref().expect("N should open dialog");
    assert_eq!(dialog.group_value(), "work");
    // The group has a member at "/tmp/work", so the path is borrowed from it
    // instead of being left on the default cwd (issue #2023).
    assert_eq!(dialog.path_value(), "/tmp/work");
}

#[test]
#[serial]
fn test_group_context_menu_new_session_prefills_path() {
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();

    // Move cursor to the "work" group row, as a right-click would.
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == "work"))
        .expect("work group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    // The group right-click menu's "New Session" routes here.
    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("NewFromSelection should open the new-session dialog");
    assert_eq!(dialog.path_value(), "/tmp/work");
    assert_eq!(dialog.group_value(), "work");
}

/// In project mode the group label is the repo basename, and the group menu's New Session
/// still borrows the member repo path; with no agents installed it points at agent setup
/// instead, like 'n'.
#[test]
#[serial]
fn test_group_context_menu_new_session_project_mode_and_no_agents() {
    use crate::session::config::GroupByMode;
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { name, .. } if name == "work"))
        .expect("work project group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("NewFromSelection should open the new-session dialog");
    assert_eq!(
        dialog.path_value(),
        "/tmp/work",
        "project-mode prefill should borrow the member repo path"
    );

    env.view.new_dialog = None;
    env.view.available_tools = AvailableTools::with_tools(&[]);
    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    assert!(
        env.view.new_dialog.is_none(),
        "no agents means the new-session form must not open"
    );
    assert!(env.view.no_agents_dialog.is_some());
}

#[test]
#[serial]
fn test_session_context_menu_new_session_prefills_from_session() {
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();

    // Move cursor onto the "work-project" session row, as a right-click would.
    let target_id = env
        .view
        .instances
        .values()
        .find(|i| i.repo_path() == "/tmp/work")
        .map(|i| i.id.clone())
        .expect("work-project instance should exist");
    let session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { id, .. } if *id == target_id))
        .expect("work-project session row should exist in flat_items");
    env.view.cursor = session_idx;
    env.view.update_selected();

    // The session right-click menu's "New Session" routes here, prefilling the
    // dialog from the right-clicked session's repo path and group (issue #2023).
    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("NewFromSelection should open the new-session dialog");
    assert_eq!(dialog.path_value(), "/tmp/work");
    assert_eq!(dialog.group_value(), "work");
}
