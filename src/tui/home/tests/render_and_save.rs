//! List rendering and persisted view state.

use super::*;

/// Default `RowTagMode::Branch` keeps worktree branch information visible.
#[test]
#[serial]
fn test_default_row_tag_mode_renders_branch_tag() {
    let mut inst = Instance::new("my-session", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::default());
    assert!(
        text.contains("[foo         ]"),
        "default row tag mode should show the compact branch tag: {text:?}"
    );
}

/// `RowTagMode::Auto` shows the profile short code in all-profiles view.
#[test]
#[serial]
fn test_row_tag_auto_renders_profile_in_all_profiles_view() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let instances_a = vec![Instance::new("A1", "/tmp/a")];
    let group_tree_a = GroupTree::new_with_groups(&instances_a, &[]);
    storage_a
        .update(|i, g| {
            *i = instances_a.to_vec();
            *g = group_tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    let storage_b = Storage::new_unwatched("beta").unwrap();
    let instances_b = vec![Instance::new("B1", "/tmp/b")];
    let group_tree_b = GroupTree::new_with_groups(&instances_b, &[]);
    storage_b
        .update(|i, g| {
            *i = instances_b.to_vec();
            *g = group_tree_b.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Auto;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let mut seen = 0;
    for item in &view.flat_items {
        if let Item::Session { id, .. } = item {
            let profile = view.get_instance(id).unwrap().source_profile.clone();
            let code = crate::tui::home::render::profile_short_code(&profile);
            let rendered = crate::tui::home::render::RowTag {
                content: code.clone(),
                max_width: 4,
            }
            .rendered();
            let text = rendered_row_text(&view, item);
            assert!(
                text.contains(&rendered),
                "all-view row for profile {profile} missing tag {rendered}: {text:?}"
            );
            seen += 1;
        }
    }
    assert_eq!(seen, 2, "expected both profile sessions to render");
}

/// `RowTagMode::Auto` does not render in a filtered view (profile already
/// in the list title).
#[test]
#[serial]
fn test_row_tag_auto_omits_tag_in_filtered_view() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let instances_a = vec![Instance::new("A1", "/tmp/a")];
    let group_tree_a = GroupTree::new_with_groups(&instances_a, &[]);
    storage_a
        .update(|i, g| {
            *i = instances_a.to_vec();
            *g = group_tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("alpha".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Auto;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let code = crate::tui::home::render::profile_short_code("alpha");
    let rendered = crate::tui::home::render::RowTag {
        content: code,
        max_width: 4,
    }
    .rendered();
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                !text.contains(&rendered),
                "Auto in filtered view should omit the tag: {text:?}"
            );
        }
    }
}

/// `RowTagMode::Profile` renders the profile tag in BOTH views (unlike
/// Auto which gates on all-profiles view).
#[test]
#[serial]
fn test_row_tag_profile_renders_in_filtered_view() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let instances_a = vec![Instance::new("A1", "/tmp/a")];
    let group_tree_a = GroupTree::new_with_groups(&instances_a, &[]);
    storage_a
        .update(|i, g| {
            *i = instances_a.to_vec();
            *g = group_tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("alpha".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Profile;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let code = crate::tui::home::render::profile_short_code("alpha");
    let rendered = crate::tui::home::render::RowTag {
        content: code,
        max_width: 4,
    }
    .rendered();
    let mut seen = 0;
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                text.contains(&rendered),
                "Profile mode should always render the tag: {text:?}"
            );
            seen += 1;
        }
    }
    assert!(seen > 0);
}

/// `RowTagMode::Branch` owns the branch suffix. It renders a compact tag even
/// when the title differs from the branch, with no raw hardcoded branch suffix.
#[test]
#[serial]
fn test_row_tag_branch_renders_when_branch_differs_from_title() {
    let mut inst = Instance::new("my-session", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::Branch);
    assert!(
        text.contains("[foo         ]"),
        "Branch mode should render the compact branch tag: {text:?}"
    );
    assert!(
        !text.contains("feature/foo"),
        "Branch mode should not render the old raw branch suffix: {text:?}"
    );
}

/// `RowTagMode::Branch` renders the tag when title matches branch.
#[test]
#[serial]
fn test_row_tag_branch_renders_when_title_matches_branch() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage = Storage::new_unwatched("alpha").unwrap();
    // Title and branch MATCH, so the divergence display stays quiet.
    let mut inst = Instance::new("feature/foo", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let instances = vec![inst];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Branch;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // The tag uses the last `/`-segment of the branch and pads to the branch
    // tag width so the row layout stays stable.
    let rendered = crate::tui::home::render::RowTag {
        content: "foo".to_string(),
        max_width: 12,
    }
    .rendered();
    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                text.contains(&rendered),
                "Branch mode must render the tag when divergence display is quiet: {text:?}"
            );
        }
    }
}

#[test]
#[serial]
fn test_row_tag_none_hides_worktree_branch_suffix() {
    let mut inst = Instance::new("my-session", "/tmp/a");
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature/foo".to_string(),
        main_repo_path: "/tmp/a-main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::None);
    assert!(
        !text.contains("feature/foo") && !text.contains("[foo") && !text.contains('['),
        "None mode should hide all worktree suffix metadata: {text:?}"
    );
}

#[test]
#[serial]
fn test_row_tag_none_hides_workspace_suffix() {
    let mut inst = Instance::new("workspace-session", "/tmp/workspace");
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/foo".to_string(),
        workspace_dir: "/tmp/workspace".to_string(),
        repos: vec![
            crate::session::WorkspaceRepo {
                name: "api".to_string(),
                source_path: "/src/api".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/api".to_string(),
                main_repo_path: "/src/api".to_string(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            },
            crate::session::WorkspaceRepo {
                name: "web".to_string(),
                source_path: "/src/web".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/web".to_string(),
                main_repo_path: "/src/web".to_string(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            },
        ],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::None);
    assert!(
        !text.contains("feature/foo") && !text.contains("repos") && !text.contains('['),
        "None mode should hide all workspace suffix metadata: {text:?}"
    );
}

#[test]
#[serial]
fn test_row_tag_branch_renders_workspace_branch_repo_count() {
    let mut inst = Instance::new("workspace-session", "/tmp/workspace");
    inst.workspace_info = Some(crate::session::WorkspaceInfo {
        branch: "feature/foo".to_string(),
        workspace_dir: "/tmp/workspace".to_string(),
        repos: vec![
            crate::session::WorkspaceRepo {
                name: "api".to_string(),
                source_path: "/src/api".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/api".to_string(),
                main_repo_path: "/src/api".to_string(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            },
            crate::session::WorkspaceRepo {
                name: "web".to_string(),
                source_path: "/src/web".to_string(),
                branch: "feature/foo".to_string(),
                worktree_path: "/tmp/workspace/web".to_string(),
                main_repo_path: "/src/web".to_string(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: None,
                base_branch_override: None,
            },
        ],
        created_at: chrono::Utc::now(),
        cleanup_on_delete: true,
    });

    let text = rendered_single_session_text(inst, crate::session::config::RowTagMode::Branch);
    assert!(
        text.contains("[foo+2       ]"),
        "Branch mode should render compact workspace branch and repo count: {text:?}"
    );
}

/// Legacy `Instance::new` left `source_profile` empty before the per-profile
/// plumbing landed. The render branch must skip the tag entirely in that
/// case rather than emit a literal `  []`.
#[test]
#[serial]
fn test_row_tag_auto_skips_for_empty_source_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage = Storage::new_unwatched("legacy").unwrap();
    let mut inst = Instance::new("Legacy1", "/tmp/legacy");
    inst.source_profile = String::new();
    let instances = vec![inst];
    let group_tree = GroupTree::new_with_groups(&instances, &[]);
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.row_tag_mode = crate::session::config::RowTagMode::Auto;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    for item in &view.flat_items {
        if let Item::Session { .. } = item {
            let text = rendered_row_text(&view, item);
            assert!(
                !text.contains("[]"),
                "row with empty source_profile must not render a literal []: {text:?}"
            );
        }
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
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
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
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
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

#[test]
#[serial]
fn test_create_profile_rejects_reserved_name_all() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let _storage = Storage::new_unwatched("default").unwrap();

    let result = crate::session::create_profile("all");
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("reserved"),
        "error should mention 'reserved'"
    );

    // Case-insensitive
    let result = crate::session::create_profile("ALL");
    assert!(result.is_err());
}

#[test]
#[serial]
fn test_delete_group_scoped_to_owning_profile() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // Create alpha with group "work"
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut inst_a = Instance::new("A1", "/tmp/a");
    inst_a.group_path = "work".to_string();
    let tree_a = GroupTree::new_with_groups(&[inst_a.clone()], &[]);
    storage_a
        .update(|i, g| {
            *i = [inst_a].to_vec();
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    // Create beta with the same group name "work"
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

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Both profiles should have a "work" group
    assert!(view.group_trees.get("alpha").unwrap().group_exists("work"));
    assert!(view.group_trees.get("beta").unwrap().group_exists("work"));

    // Find a "work" group item that belongs to alpha and select it.
    // Collect candidate indices first to avoid borrow conflicts.
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
        if view.selected_group_profile.as_deref() == Some("alpha") {
            break;
        }
    }

    assert_eq!(view.selected_group.as_deref(), Some("work"));
    assert_eq!(view.selected_group_profile.as_deref(), Some("alpha"));

    // Delete alpha's "work" group
    view.delete_selected_group().unwrap();

    // Alpha's "work" group should be gone, but beta's should remain
    assert!(
        !view.group_trees.get("alpha").unwrap().group_exists("work"),
        "alpha's 'work' group should be deleted"
    );
    assert!(
        view.group_trees.get("beta").unwrap().group_exists("work"),
        "beta's 'work' group should be untouched"
    );

    // Alpha's instance should be ungrouped, beta's should still be in "work"
    let alpha_inst = view
        .instances()
        .find(|i| i.source_profile == "alpha")
        .unwrap();
    assert_eq!(
        alpha_inst.group_path, "",
        "alpha's instance should be ungrouped"
    );
    let beta_inst = view
        .instances()
        .find(|i| i.source_profile == "beta")
        .unwrap();
    assert_eq!(
        beta_inst.group_path, "work",
        "beta's instance should still be in 'work'"
    );
}

/// Opening the group-delete dialog must scope its session count to the
/// selected group's profile. Two profiles can own a same-named group; an
/// empty group in one profile should open the simple confirm, not the
/// "delete N sessions" options dialog driven by its populated twin in
/// another profile. Regression for the group-key conflict where the empty
/// group was not the one the delete modal acted on.
#[test]
#[serial]
fn test_group_delete_dialog_scoped_to_owning_profile() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    // alpha owns an EMPTY "work" group (group exists, no sessions).
    let storage_a = Storage::new_unwatched("alpha").unwrap();
    let mut tree_a = GroupTree::new_with_groups(&[], &[]);
    tree_a.create_group("work");
    storage_a
        .update(|i, g| {
            *i = vec![];
            *g = tree_a.get_all_groups();
            Ok(())
        })
        .unwrap();

    // beta owns a same-named "work" group that still has a session.
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

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Select alpha's (empty) "work" group.
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
        if view.selected_group_profile.as_deref() == Some("alpha") {
            break;
        }
    }
    assert_eq!(view.selected_group_profile.as_deref(), Some("alpha"));

    view.open_delete_for_selected();

    assert!(
        view.group_delete_options_dialog.is_none(),
        "empty group must not trigger the with-sessions options dialog from a same-named group in another profile"
    );
    assert!(
        view.confirm_dialog.is_some(),
        "empty group should open the simple delete-group confirm"
    );
}

// Four rename-collision behaviors (untied duplicate-pair reject, group-only
// change allowed, tied derived-destination collision, cross-profile target
// collision) share one test because they need the same `#[serial]`-forcing
// setup: an isolated home, multiple `HomeView`/`Storage` instances, and a
// process-global `tie_workdir_to_name` flip. Splitting would multiply that
// setup and the serial critical-path time; each behavior asserts independently.
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

    let mut view = HomeView::new(
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
    let mut unified = HomeView::new(
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

/// Changing a session's profile via the rename dialog must transfer its group
/// metadata in the same storage transaction. Otherwise the source can reload
/// an empty duplicate while the target row renders under a separately-created
/// group.
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
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
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

#[test]
#[serial]
fn test_group_context_menu_new_session_shows_no_agents_without_tools() {
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();
    env.view.available_tools = AvailableTools::with_tools(&[]);

    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == "work"))
        .expect("work group should exist in flat_items");
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view
        .dispatch_context_menu_action(ContextMenuAction::NewFromSelection);
    assert!(
        env.view.new_dialog.is_none(),
        "no agents means the new-session form must not open"
    );
    assert!(
        env.view.no_agents_dialog.is_some(),
        "should point the user at agent setup instead, like 'n'"
    );
}

#[test]
#[serial]
fn test_group_context_menu_new_session_prefills_path_in_project_mode() {
    use crate::session::config::GroupByMode;
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // In project mode the group label is the repo basename ("work" from
    // "/tmp/work"), not the stored group_path.
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
