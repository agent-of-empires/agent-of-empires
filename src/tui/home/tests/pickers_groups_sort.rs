//! Pickers, settings entry, group management, layout, and sort.

use super::*;

#[test]
#[serial]
fn test_uppercase_p_picker_switch_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    crate::session::create_profile("first").unwrap();
    crate::session::create_profile("second").unwrap();
    crate::session::create_profile("third").unwrap();

    let _storage = Storage::new_unwatched("first").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new_for_test(
        Some("first".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    view.handle_key(key(KeyCode::Char('P')), None);
    assert!(view.profile_picker_dialog.is_some());
    view.handle_key(key(KeyCode::Esc), None);
    assert!(view.profile_picker_dialog.is_none());

    view.handle_key(key(KeyCode::Char('P')), None);
    // The active profile is selected; a trailing entry prevents end-of-list clamping.
    view.handle_key(key(KeyCode::Down), None);
    let action = view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(action, None);
    assert_eq!(view.active_profile, Some("second".to_string()));
    assert!(view.profile_picker_dialog.is_none());
}

/// Every overlay that takes the keyboard registers with `has_dialog`, which the mouse,
/// scroll and shortcut gates all read.
#[test]
#[serial]
fn test_has_dialog_includes_overlays() {
    let env = create_test_env_empty();
    let mut view = env.view;
    assert!(!view.has_dialog());

    view.info_dialog = Some(InfoDialog::new("Test", "Test message"));
    assert!(view.has_dialog(), "info dialog");
    view.info_dialog = None;

    view.handle_key(key(KeyCode::Char('s')), None);
    assert!(view.settings_view.is_some(), "`s` opens settings");
    assert!(view.has_dialog(), "settings view");
}

/// `t` toggles Structured/Terminal view. Shift+T attaches the paired terminal from either
/// view without changing the view mode, so it stays the one-key path to the shell; in
/// Terminal view Enter attaches the terminal and `d` explains instead of deleting.
#[test]
#[serial]
fn test_terminal_view_keys() {
    {
        let mut empty = create_test_env_empty();
        assert!(
            empty
                .view
                .handle_key(key(KeyCode::Char('T')), None)
                .is_none(),
            "Shift+T with no sessions is a no-op"
        );
    }

    let env = create_test_env_with_sessions(1);
    let mut view = env.view;
    assert_eq!(view.view_mode, ViewMode::Structured);
    let action = view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachSession(_))));
    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
    assert_eq!(view.view_mode, ViewMode::Structured);

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);
    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
    assert_eq!(view.view_mode, ViewMode::Terminal);
    let action = view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));

    assert!(view.info_dialog.is_none());
    view.handle_key(key(KeyCode::Char('d')), None);
    assert!(view.info_dialog.is_some());
    assert!(view.unified_delete_dialog.is_none());
    view.info_dialog = None;

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Structured);
}

#[test]
#[serial]
fn switching_view_retargets_capture_worker_pane() {
    // The capture worker follows the displayed pane, so switching agent to terminal must
    // resolve to different tmux sessions and make `sync_preview_capture_worker` respawn
    // against the new pane. Guards the fix that moved `capture-pane` off the render
    // thread.
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    let agent_pane = view.displayed_pane_tmux_name();
    assert!(
        agent_pane.is_some(),
        "a selected session must resolve a pane"
    );

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);
    let terminal_pane = view.displayed_pane_tmux_name();
    assert!(terminal_pane.is_some());
    assert_ne!(
        agent_pane, terminal_pane,
        "agent and terminal panes must differ so the worker retargets on switch",
    );

    // The reconcile tracks the active target and is idempotent: a changed
    // pane updates it, the same pane leaves it in place.
    view.sync_preview_capture_worker(terminal_pane.clone());
    assert_eq!(view.preview_capture_target, terminal_pane);
    view.sync_preview_capture_worker(terminal_pane.clone());
    assert_eq!(view.preview_capture_target, terminal_pane);
    view.sync_preview_capture_worker(agent_pane.clone());
    assert_eq!(view.preview_capture_target, agent_pane);
}

#[test]
#[serial]
fn retarget_same_session_tool_clears_previous_pane_content() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;
    view.view_mode = ViewMode::Tool("lazygit".to_string());
    view.sync_preview_capture_worker(Some("aoe_test_tool_a".to_string()));
    view.tool_preview_cache.content = "tool A screen".to_string();
    view.tool_preview_cache.capture_target = Some("aoe_test_tool_a".to_string());
    view.tool_preview_cache.session_id = view.selected_session.clone();

    view.sync_preview_capture_worker(Some("aoe_test_tool_b".to_string()));

    assert!(
        view.tool_preview_cache.content.is_empty(),
        "a cold pane must not render another tool's bytes from the same session",
    );
    assert!(view.tool_preview_cache.capture_target.is_none());
}

/// Trashing and restoring through the view's own actions keeps the group header count in
/// step with the rows, since both rebuild from the same predicate.
#[test]
#[serial]
fn group_header_count_tracks_trash_and_restore() {
    let mut env = create_test_env_with_group_sessions();
    env.view.trashed_section_collapsed = false;

    let work_count = |env: &TestEnv| -> usize {
        env.view
            .flat_items
            .iter()
            .find_map(|i| match i {
                Item::Group {
                    path,
                    session_count,
                    ..
                } if path == "work" => Some(*session_count),
                _ => None,
            })
            .expect("work group header present")
    };

    // "work" holds two direct sessions plus one in the nested "work/projects".
    assert_eq!(work_count(&env), 3);

    let target = env
        .view
        .instances
        .values()
        .find(|i| i.group_path == "work")
        .map(|i| i.id.clone())
        .expect("a direct work session");

    env.view.trash_session_by_id(&target);
    assert_eq!(
        work_count(&env),
        2,
        "trashed session drops out of the count"
    );

    env.view.select_session_by_id(&target);
    env.view.toggle_archive_at_cursor().unwrap();
    assert_eq!(work_count(&env), 3, "restored session returns to the count");
}

#[test]
#[serial]
fn test_group_has_managed_worktrees_and_containers() {
    let mut worktree = instance_in("work-session", "/tmp/work", "work");
    worktree.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature-branch".to_string(),
        main_repo_path: "/tmp/main".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let mut sandboxed = instance_in("box-session", "/tmp/box", "box");
    sandboxed.sandbox_info = Some(crate::session::SandboxInfo {
        provider: None,
        enabled: true,
        container_id: None,
        image: "ubuntu:latest".to_string(),
        container_name: "test-container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });
    let env = seeded_env(test_home(), &[worktree, sandboxed], true);

    assert!(env.view.group_has_managed_worktrees("work", "work/", None));
    assert!(!env.view.group_has_managed_worktrees("box", "box/", None));
    assert!(env.view.group_has_containers("box", "box/", None));
    assert!(!env.view.group_has_containers("work", "work/", None));
}

#[test]
#[serial]
fn test_delete_selected_group_updates_groups_field() {
    let mut env = create_test_env_with_group_sessions();

    // Select the "work" group
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    assert!(env.view.selected_group.is_some());
    assert!(env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work"));

    // Delete the group (this moves sessions to default)
    env.view.delete_selected_group().unwrap();

    // Verify the group is removed from group_tree
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work"));

    // Verify self.groups is updated (this is the bug fix)
    let all_groups = env.view.all_groups();
    let group_paths: Vec<_> = all_groups.iter().map(|g| g.path.as_str()).collect();
    assert!(!group_paths.contains(&"work"));
    assert!(!group_paths.contains(&"work/projects"));

    // A reload from storage agrees with the in-memory tree.
    env.view.reload().unwrap();
    let reloaded_groups: Vec<_> = env
        .view
        .all_groups()
        .iter()
        .map(|g| g.path.clone())
        .collect();
    let tree_groups: Vec<_> = env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .get_all_groups()
        .iter()
        .map(|g| g.path.clone())
        .collect();
    assert_eq!(reloaded_groups, tree_groups);
}

/// Archiving a manual group archives every session under it, including
/// nested subgroups, and leaves sessions outside the group untouched.
#[test]
#[serial]
fn test_archive_selected_group_archives_all_members() {
    let mut env = create_test_env_with_group_sessions();

    // Select the "work" group.
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }
    assert_eq!(env.view.selected_group.as_deref(), Some("work"));

    // "work" holds two direct sessions plus one in the nested "work/projects".
    assert_eq!(env.view.active_sessions_in_selected_group().len(), 3);

    env.view.archive_selected_group().unwrap();

    for inst in env.view.instances() {
        let in_work = inst.group_path == "work" || inst.group_path.starts_with("work/");
        assert_eq!(
            inst.is_archived(),
            in_work,
            "session {} (group {:?}) archived state should match group membership",
            inst.title,
            inst.group_path
        );
    }
}

/// Locks #1868: bulk archive persists synchronously even though tmux teardown runs
/// off-thread. Real tmux state is asserted in `tests/e2e/archive_restore.rs`.
#[test]
#[serial]
fn test_archive_selected_group_widened_teardown_persists_synchronously() {
    let mut env = create_test_env_with_group_sessions();

    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }
    assert_eq!(env.view.selected_group.as_deref(), Some("work"));
    let work_ids: Vec<String> = env.view.active_sessions_in_selected_group();
    assert_eq!(work_ids.len(), 3);

    let result = env.view.archive_selected_group();
    assert!(
        result.is_ok(),
        "archive_selected_group must return Ok even when the off-thread \
         teardown is fire-and-forget; got {:?}",
        result
    );

    for id in &work_ids {
        let inst = env
            .view
            .instances()
            .find(|i| &i.id == id)
            .expect("group member must still exist after archive");
        assert!(
            inst.is_archived(),
            "session {} ({}) must have archived_at set synchronously \
             on the input thread before archive_selected_group returns",
            inst.title,
            id
        );
    }
}

/// In project mode, archiving a project header archives every live session mapping to that
/// repo, even though their stored `group_path` differs from the synthetic project name.
#[test]
#[serial]
fn test_archive_selected_group_project_mode() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    // Two sessions sharing one repo, one session in a different repo.
    let a1 = Instance::new("alpha-1", "/tmp/alpha");
    let a2 = Instance::new("alpha-2", "/tmp/alpha");
    let b1 = Instance::new("beta-1", "/tmp/beta");
    let instances = vec![a1, a2, b1];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new_for_test(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Project;
    view.flat_items = view.build_flat_items();

    // Select the "alpha" project header.
    for (i, item) in view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "alpha" {
                view.cursor = i;
                view.update_selected();
                break;
            }
        }
    }
    assert_eq!(view.selected_group.as_deref(), Some("alpha"));
    assert_eq!(view.active_sessions_in_selected_group().len(), 2);

    view.archive_selected_group().unwrap();

    for inst in view.instances() {
        let in_alpha = inst.project_path == "/tmp/alpha";
        assert_eq!(
            inst.is_archived(),
            in_alpha,
            "session {} (repo {}) archived state should match project membership",
            inst.title,
            inst.project_path
        );
    }
}

/// The group-level prompt opens a confirmation carrying the `archive_group` action and
/// counts only active members, and no-ops silently when nothing is left to archive.
#[test]
#[serial]
fn test_prompt_archive_selected_group() {
    let mut env = create_test_env_with_group_sessions();

    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    env.view.prompt_archive_selected_group();
    assert_eq!(
        env.view.confirm_dialog.as_ref().map(|d| d.action()),
        Some("archive_group")
    );

    // Confirm, which archives the group and clears the prompt.
    env.view.confirm_dialog = None;
    env.view.archive_selected_group().unwrap();

    // With every member archived, a second prompt is a silent no-op.
    env.view.prompt_archive_selected_group();
    assert!(env.view.confirm_dialog.is_none());
}

#[test]
#[serial]
fn test_delete_group_with_sessions_updates_groups_field() {
    use crate::tui::dialogs::GroupDeleteOptions;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();
    let other_storage = Storage::new_unwatched("other").unwrap();

    let project = temp.path().join("work");
    std::fs::create_dir(&project).unwrap();
    let mut hidden = Instance::new("hidden-trash-member", &project.to_string_lossy());
    hidden.group_path = "work/projects".to_string();
    hidden.trash();
    hidden.lifecycle_generation = 1;
    hidden.lifecycle_reservation = Some(LifecycleReservation {
        op: LifecycleOperation::Launch,
        generation: 1,
        at: chrono::Utc::now(),
    });
    storage
        .update(|instances, groups| {
            instances.push(hidden);
            groups.extend([
                Group::new("work", "work"),
                Group::new("projects", "work/projects"),
                Group::new("workbench", "workbench"),
            ]);
            Ok(())
        })
        .unwrap();
    other_storage
        .update(|_, groups| {
            groups.extend([
                Group::new("work", "work"),
                Group::new("projects", "work/projects"),
            ]);
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new_for_test(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    for (i, item) in view.flat_items.iter().enumerate() {
        if let Item::Group {
            path,
            session_count,
            ..
        } = item
        {
            if path == "work" {
                assert_eq!(*session_count, 0, "trashed members stay hidden");
                view.cursor = i;
                view.update_selected();
                break;
            }
        }
    }
    assert_eq!(view.selected_group.as_deref(), Some("work"));
    assert_eq!(view.selected_group_profile.as_deref(), Some("test"));

    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: false,
        delete_branches: false,
        delete_containers: false,
        force_delete_worktrees: false,
    };
    view.delete_group_with_sessions(&options).unwrap();
    view.save().unwrap();
    let during_delete = storage.load().unwrap();
    assert_eq!(during_delete.len(), 1);
    assert_ne!(
        during_delete[0].status,
        Status::Deleting,
        "save persisted the transient Deleting status"
    );

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !view.apply_deletion_results() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    assert!(
        view.info_dialog.is_some(),
        "busy purge result was not delivered"
    );

    let persisted = storage.load().unwrap();
    assert_eq!(persisted.len(), 1);
    assert!(persisted[0].group_path.is_empty());
    assert_ne!(persisted[0].status, Status::Deleting);
    storage
        .update(|instances, _| {
            instances.clear();
            Ok(())
        })
        .unwrap();

    view.reload().unwrap();
    let tree = view.group_trees.get("test").unwrap();
    assert!(!tree.group_exists("work"));
    assert!(!tree.group_exists("work/projects"));
    assert!(tree.group_exists("workbench"), "near-prefix group removed");

    let (_, groups) = storage.load_with_groups().unwrap();
    assert!(!groups
        .iter()
        .any(|group| group.path == "work" || group.path.starts_with("work/")));
    assert!(groups.iter().any(|group| group.path == "workbench"));
    let (_, other_groups) = other_storage.load_with_groups().unwrap();
    assert!(other_groups.iter().any(|group| group.path == "work"));
    assert!(other_groups
        .iter()
        .any(|group| group.path == "work/projects"));
    let mut creating = Instance::new("creating-member", "/tmp/creating");
    creating.source_profile = "test".to_string();
    creating.group_path = "creating".to_string();
    creating.status = Status::Creating;
    let creating_id = creating.id.clone();
    view.add_instance(creating);
    view.rebuild_group_trees();
    view.selected_group = Some("creating".to_string());
    view.selected_group_profile = Some("test".to_string());
    view.info_dialog = None;

    view.delete_group_with_sessions(&options).unwrap();
    assert_eq!(view.selected_group.as_deref(), Some("creating"));
    assert_eq!(
        view.info_dialog.as_ref().map(InfoDialog::title),
        Some("Creation in progress")
    );

    view.mutate_instance(&creating_id, |instance| {
        instance.status = Status::Deleting;
    });
    view.save().unwrap();
    assert!(!storage
        .load()
        .unwrap()
        .iter()
        .any(|instance| instance.id == creating_id));
}

#[test]
#[serial]
fn test_group_collapsed_state_persists_across_reload() {
    let mut env = create_test_env_with_groups();

    let (group_idx, group_path) = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .find_map(|(i, item)| match item {
            Item::Group {
                path, collapsed, ..
            } => {
                assert!(!collapsed, "group should start expanded");
                Some((i, path.clone()))
            }
            _ => None,
        })
        .expect("should have a group");

    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);
    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(*collapsed, "group should be collapsed after Enter");
    }

    let (_, groups) = env
        .view
        .storages
        .get("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let fresh_tree =
        GroupTree::new_with_groups(&env.view.instances().cloned().collect::<Vec<_>>(), &groups);
    assert!(
        fresh_tree
            .get_all_groups()
            .iter()
            .find(|g| g.path == group_path)
            .expect("group should exist in storage")
            .collapsed,
        "collapsed state should be persisted to storage"
    );

    // Reload (simulates the periodic refresh); the group index may change.
    env.view.reload().unwrap();
    let still_collapsed = env.view.flat_items.iter().find_map(|item| match item {
        Item::Group {
            path, collapsed, ..
        } if path == &group_path => Some(*collapsed),
        _ => None,
    });
    assert_eq!(
        still_collapsed,
        Some(true),
        "group should remain collapsed after reload"
    );
}

/// Project and org folder collapse must survive a restart. Both kinds of header are
/// auto-derived and have no group record, so their state is written to `app_state` rather
/// than the per-profile GroupTree, and a fresh `HomeView` restores it. A collapse entry for
/// a folder that no longer exists is pruned on save, so the persisted set cannot grow
/// without bound.
#[test]
#[serial]
fn test_derived_group_collapsed_state_persists_to_config() {
    use crate::session::config::GroupByMode;

    for mode in [GroupByMode::Project, GroupByMode::Org] {
        let mut env = create_test_env_two_projects_mixed_attention();
        env.view.group_by = mode;
        env.view.flat_items = env.view.build_flat_items();

        let (group_idx, group_path) = env
            .view
            .flat_items
            .iter()
            .enumerate()
            .find_map(|(idx, item)| match item {
                Item::Group {
                    path, collapsed, ..
                } => {
                    assert!(!collapsed, "{mode:?} folder should start expanded");
                    Some((idx, path.clone()))
                }
                _ => None,
            })
            .expect("a derived folder header");

        let saved_paths = || {
            let config = crate::session::config::load_config()
                .unwrap()
                .expect("config should exist after collapse");
            match mode {
                GroupByMode::Project => config.app_state.project_group_collapsed,
                _ => config.app_state.org_group_collapsed,
            }
        };

        // Enter routes through toggle_group_collapsed, which persists.
        env.view.cursor = group_idx;
        env.view.update_selected();
        env.view.handle_key(key(KeyCode::Enter), None);
        assert!(
            saved_paths().contains(&group_path),
            "{mode:?}: the collapsed folder path should be persisted to app_state"
        );

        let fresh = HomeView::new_for_test(
            Some("test".to_string()),
            AvailableTools::with_tools(&["claude"]),
            crate::file_watch::FileWatchService::noop(),
        )
        .unwrap();
        let restored = match mode {
            GroupByMode::Project => fresh.project_group_collapsed.get(&group_path),
            _ => fresh.org_group_collapsed.get(&group_path),
        };
        assert_eq!(
            restored.copied(),
            Some(true),
            "{mode:?}: a relaunched HomeView should restore the collapsed folder"
        );

        match mode {
            GroupByMode::Project => {
                env.view
                    .project_group_collapsed
                    .insert("/repos/deleted-ghost".to_string(), true);
                env.view.save_project_group_collapsed();
            }
            _ => {
                env.view
                    .org_group_collapsed
                    .insert("/repos/deleted-ghost".to_string(), true);
                env.view.save_org_group_collapsed();
            }
        }
        let saved = saved_paths();
        assert!(
            saved.contains(&group_path),
            "{mode:?}: a live collapsed folder must stay persisted"
        );
        assert!(
            !saved.iter().any(|p| p == "/repos/deleted-ghost"),
            "{mode:?}: a collapse entry for a nonexistent folder must be pruned"
        );
    }
}

/// `<` / `>` move the divider left / right by 5 from its default, so with the sidebar on
/// the right they grow / shrink the list, and the width clamps at the 10 / 80 bounds.
#[test]
#[serial]
fn test_list_width_steps_and_clamps() {
    use crate::session::config::SidebarPosition;
    // (sidebar position, width after `<`, width after `<` then `>` twice)
    for (position, after_left, after_right) in [
        (SidebarPosition::Left, 30, 40),
        (SidebarPosition::Right, 40, 30),
    ] {
        let mut env = create_test_env_empty();
        env.view.sidebar_position = position;
        assert_eq!(env.view.list_width, 35);
        env.view.handle_key(key(KeyCode::Char('<')), None);
        assert_eq!(env.view.list_width, after_left, "{position:?}: `<`");
        env.view.handle_key(key(KeyCode::Char('>')), None);
        env.view.handle_key(key(KeyCode::Char('>')), None);
        assert_eq!(env.view.list_width, after_right, "{position:?}: `>`");
    }

    let mut env = create_test_env_empty();

    env.view.list_width = 12;
    env.view.shrink_list();
    env.view.shrink_list();
    assert_eq!(env.view.list_width, 10, "shrink clamps at the minimum");

    env.view.list_width = 78;
    env.view.grow_list();
    env.view.grow_list();
    assert_eq!(env.view.list_width, 80, "grow clamps at the maximum");
}

/// The picker must not offer a repo the session already has, since the attach would be
/// rejected as a duplicate. With no registry entries there is nothing to offer and the
/// dialog says so rather than rendering an empty list. The picker is a modal, so it must
/// register in the overlay predicates gating scroll, right-click, footer clicks, drag start
/// and paste-burst routing; missing from them, the wheel moved the cursor underneath it and
/// right-click stacked a second menu on top.
#[test]
#[serial]
fn add_project_picker_opens_and_excludes_repos_already_on_the_session() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    assert!(!env.view.has_dialog(), "no dialog open yet");

    env.view.open_add_project_for_selected();
    let dialog = env
        .view
        .attach_project_dialog
        .as_ref()
        .expect("picker should open for a selected session");
    assert_eq!(dialog.session_id(), id);
    // The fixture registers no projects, so every candidate is filtered out or
    // absent; either way the picker reports that rather than showing a list.
    assert!(dialog.is_empty());
    assert!(env.view.has_dialog());
    assert!(env.view.has_non_live_send_overlay());

    // Esc closes without attaching.
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.attach_project_dialog.is_none());
    assert!(!env.view.has_dialog());
    assert!(!env.view.has_non_live_send_overlay());
    assert!(
        env.view
            .get_instance(&id)
            .is_some_and(|i| i.all_repos().is_empty()),
        "cancelling must not attach anything"
    );
}

/// Attaching bounces the worker and creates a worktree, so the picker refuses the same
/// lifecycle states every sibling mutator refuses. The context menu offers the row
/// unconditionally, so this gate is the only thing stopping an archived or mid-turn
/// session. A scratch session has no repo of its own, so it is refused too rather than
/// opening a list where every choice would be rejected by `attach_project::plan`.
#[test]
#[serial]
fn add_project_picker_refuses_shelved_and_mid_turn_sessions() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // `Waiting` and `Starting` are turns in flight too, which is why the gate reuses
    // `Status::blocks_worktree_edit()`: SIGTERMing a `Waiting` worker throws away a pending
    // approval.
    for status in [
        crate::session::Status::Creating,
        crate::session::Status::Deleting,
        crate::session::Status::Running,
        crate::session::Status::Waiting,
        crate::session::Status::Starting,
    ] {
        env.view.mutate_instance(&id, |inst| inst.status = status);
        env.view.info_dialog = None;
        env.view.open_add_project_for_selected();
        assert!(
            env.view.attach_project_dialog.is_none(),
            "picker must not open for status {status:?}"
        );
        assert!(
            env.view.info_dialog.is_some(),
            "the refusal must be visible for status {status:?}, not a silent no-op"
        );
    }

    // Archived: agent is deliberately stopped, so a worktree here reads nothing.
    env.view
        .mutate_instance(&id, |inst| inst.status = crate::session::Status::Idle);
    env.view.mutate_instance(&id, |inst| inst.archive());
    env.view.info_dialog = None;
    env.view.open_add_project_for_selected();
    assert!(env.view.attach_project_dialog.is_none());
    assert!(env.view.info_dialog.is_some());

    env.view.mutate_instance(&id, |inst| {
        inst.unarchive();
        inst.scratch = true;
    });
    env.view.info_dialog = None;
    env.view.open_add_project_for_selected();
    assert!(env.view.attach_project_dialog.is_none());
    assert!(env.view.info_dialog.is_some());

    // Idle, unshelved and not scratch: the picker opens.
    env.view.mutate_instance(&id, |inst| inst.scratch = false);
    env.view.info_dialog = None;
    env.view.open_add_project_for_selected();
    assert!(
        env.view.attach_project_dialog.is_some(),
        "an idle, unshelved session must be attachable"
    );
}

/// The attach runs on a background poller, so dispatch must return without touching git:
/// `git worktree add` plus a fetch and submodule init on the render thread froze the UI for
/// the whole attach. A second dispatch for the same session is refused, since it would race
/// the first one's worktree creation and worker bounce.
#[test]
#[serial]
fn add_project_dispatches_to_the_poller_and_refuses_a_second_attach() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();

    let dispatched = env
        .view
        .add_project_to_session(&id, std::path::Path::new("/tmp/some-repo"));
    assert!(dispatched.is_ok(), "dispatch must not block on the attach");
    assert!(
        env.view.attach_project_in_flight.contains(&id),
        "the in-flight marker is what suppresses a concurrent attach"
    );
    assert!(
        env.view
            .get_instance(&id)
            .is_some_and(|i| i.all_repos().is_empty()),
        "nothing is recorded until the worker reports back"
    );

    let second = env
        .view
        .add_project_to_session(&id, std::path::Path::new("/tmp/other-repo"));
    assert!(second.is_err(), "a second attach must be refused");
    assert!(format!("{:#}", second.unwrap_err()).contains("already running"));
}

/// The completion path clears the marker and replaces the progress dialog for both
/// outcomes; without the clear, one failed attach would leave the session permanently
/// unattachable.
#[test]
#[serial]
fn apply_attach_project_results_reports_and_clears_the_marker() {
    for outcome in [
        Ok("Attached 'frontend' on branch 'feature/abc'.".to_string()),
        Err("branch 'feature/abc' already exists in the repo being attached".to_string()),
    ] {
        let expect_ok = outcome.is_ok();
        let mut env = create_test_env_with_sessions(1);
        let id = env.view.instance_at(0).id.clone();
        env.view.attach_project_in_flight.insert(id.clone());
        env.view.attach_project_poller =
            crate::tui::attach_project_poller::AttachProjectPoller::with_result_for_test(
                crate::tui::attach_project_poller::AttachProjectResult {
                    session_id: id.clone(),
                    outcome,
                },
            );

        assert!(
            env.view.apply_attach_project_results(),
            "a delivered result has to repaint"
        );
        assert!(
            !env.view.attach_project_in_flight.contains(&id),
            "the marker must clear, or the session stays unattachable forever"
        );
        let dialog = env
            .view
            .info_dialog
            .as_ref()
            .expect("the outcome must be visible, not a silent no-op");
        if expect_ok {
            assert!(
                dialog.title().contains("Attached"),
                "got {}",
                dialog.title()
            );
        } else {
            assert!(
                dialog.title().contains("Could Not Attach"),
                "got {}",
                dialog.title()
            );
        }
    }
}

#[test]
#[serial]
fn test_shift_o_opens_sort_picker_in_strict_mode() {
    // The SortPicker binding lists Shift+O for strict mode, so it must resolve to the sort
    // picker rather than falling through to the typing-guard.
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    env.view.strict_hotkeys = true;
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Shift+O: opens the sort picker.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::SHIFT), None);
    assert!(env.view.sort_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);

    // Some terminals drop the SHIFT modifier and send bare uppercase. Cover
    // that too.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('O'), KeyModifiers::NONE), None);
    assert!(env.view.sort_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);

    // Ctrl+o also opens the picker in strict mode.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        None,
    );
    assert!(env.view.sort_picker_dialog.is_some());
    env.view.handle_key(key(KeyCode::Esc), None);

    // Plain lowercase 'o' must not cycle sort in strict mode: it falls through to the
    // typing-guard per the "no destructive lowercase" rule. A single unguarded
    // `Char('o') => cycle` arm silently changed the sort whenever the user typed it as text.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), None);
    assert!(env.view.sort_picker_dialog.is_none());

    // Sort order is unchanged because no selection was confirmed.
    assert_eq!(env.view.sort_order, SortOrder::Newest);
    // Sanity: message dialog must NOT have been opened as a side effect.
    assert!(env.view.send_message_dialog.is_none());
}

#[test]
#[serial]
fn test_strict_mode_h_collapses_group() {
    // The help overlay lists "h/←" for Collapse group in strict mode, so bare `h` must walk
    // through dispatch and collapse the cursor's group, mirroring `l`/Right. Without the
    // explicit `Char('h')` arm it would fall into the typing-guard and open compose.
    let mut env = create_test_env_with_groups();
    env.view.strict_hotkeys = true;

    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("setup should produce a group");

    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(!collapsed, "group should start expanded");
    }
    env.view.cursor = group_idx;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('h')), None);

    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(
            *collapsed,
            "bare 'h' in strict mode must collapse the group"
        );
    }
    assert!(
        env.view.pending_paste.is_none(),
        "bare 'h' in strict mode must not leak into the typing-guard catch-all"
    );
}

#[test]
#[serial]
fn test_non_strict_h_snoozes_only_in_attention_sort() {
    // Snooze is Attention-only: there `h` toggles snooze on the cursor's session and the
    // group below stays expanded, while every other sort falls through to the unconditional
    // `Left | Char('h')` collapse. Before the gate, snooze caught first in non-strict mode
    // regardless of sort and silently mutated persisted state.
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_groups();
    env.view.strict_hotkeys = false;

    // Attention sort flattens groups, so seed a cursor-on-session scenario and assert that
    // `h` opens the snooze duration dialog; the snooze itself fires on the pick.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    let session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { .. }))
        .expect("setup should produce a session in Attention sort");
    env.view.cursor = session_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Char('h')), None);
    assert!(
        env.view.snooze_duration_dialog.is_some(),
        "`h` in Attention sort must open the snooze duration dialog"
    );
    // Tear the dialog back down before exercising the Newest case so the
    // next handle_key doesn't get swallowed by dialog input.
    env.view.snooze_duration_dialog = None;
    env.view.pending_snooze_session = None;

    // Now flip back to a non-Attention sort and confirm `h` falls
    // through to the collapse handler instead of snoozing.
    env.view.sort_order = SortOrder::Newest;
    env.view.flat_items = env.view.build_flat_items();
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("setup should produce a group in Newest sort");
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Char('h')), None);
    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(
            *collapsed,
            "non-strict 'h' outside Attention sort must collapse the group, not snooze"
        );
    }
}

#[test]
#[serial]
fn test_non_strict_w_jumps_to_next_waiting_in_attention_sort() {
    // #1524: in non-strict Attention sort, `w` must jump to the next waiting/idle session
    // (#796) rather than snoozing the cursor's session. Snooze lives on `h`/`H`; the snooze
    // arm used to shadow the jump arm in exactly the sort users triage in.
    use crate::session::Status;

    let (mut env, running, _waiting) = attention_env_running_then_waiting();
    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    assert!(
        env.view.snooze_duration_dialog.is_none(),
        "`w` in Attention sort must jump, not open the snooze dialog"
    );
    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => env.view.get_instance(id).map(|i| i.status),
        _ => None,
    };
    assert_eq!(
        landed,
        Some(Status::Waiting),
        "`w` should land the cursor on the Waiting session"
    );
}

#[test]
#[serial]
fn test_non_strict_w_on_running_jumps_to_idle_in_attention_sort() {
    use crate::session::Status;

    let (mut env, running, _idle) = attention_env_running_then_idle();
    env.view.cursor = running;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::Char('w')), None);

    assert!(
        env.view.snooze_duration_dialog.is_none(),
        "`w` on a Running session must jump, not open the snooze dialog"
    );
    let landed = match env.view.flat_items.get(env.view.cursor) {
        Some(Item::Session { id, .. }) => env.view.get_instance(id).map(|i| i.status),
        _ => None,
    };
    assert_eq!(
        landed,
        Some(Status::Idle),
        "`w` should fall back to the available Idle session"
    );
}

#[test]
#[serial]
fn test_non_strict_w_cycles_through_all_idle_sessions_in_attention_sort() {
    use crate::session::config::{GroupByMode, SortOrder};
    use crate::session::Status;

    let mut env = create_test_env_empty();
    env.view.strict_hotkeys = false;
    env.view.group_by = GroupByMode::Manual;
    env.view.sort_order = SortOrder::Attention;
    env.view.idle_decay_window = std::time::Duration::ZERO;

    for (index, minutes_ago) in [5, 10, 15, 20].into_iter().enumerate() {
        let title = format!("idle-{index}");
        let path = format!("/tmp/idle-{index}");
        let mut inst = Instance::new(&title, &path);
        inst.source_profile = "test".to_string();
        inst.status = Status::Idle;
        inst.last_accessed_at = Some(chrono::Utc::now() - chrono::Duration::minutes(minutes_ago));
        env.view.add_instance(inst);
    }
    env.view.flat_items = env.view.build_flat_items();
    env.view.update_selected();

    let session_ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            Item::Session { id, .. } => Some(id.clone()),
            Item::Group { .. } => None,
        })
        .collect();
    assert_eq!(session_ids.len(), 4);

    let start = session_ids[0].clone();
    env.view.select_session_by_id(&start);
    let mut expected = session_ids[1..].to_vec();
    expected.push(start);

    let mut visited = Vec::new();
    for _ in 0..expected.len() {
        env.view.handle_key(key(KeyCode::Char('w')), None);
        visited.push(
            env.view
                .selected_session
                .clone()
                .expect("w should select an idle session"),
        );
    }

    assert_eq!(
        visited, expected,
        "repeated w presses must walk idle rows in list order"
    );
}

#[test]
#[serial]
fn test_non_strict_w_on_collapsed_project_group_reveals_idle_in_attention_sort() {
    use crate::session::config::{GroupByMode, SortOrder};
    use crate::session::Status;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut alpha_idle = Instance::new("alpha-idle", "/repos/alpha");
    alpha_idle.status = Status::Idle;
    let alpha_id = alpha_idle.id.clone();
    let mut beta_running = Instance::new("beta-running", "/repos/beta");
    beta_running.status = Status::Running;
    let instances = vec![alpha_idle, beta_running];
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = GroupTree::new_with_groups(&instances, &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let mut view = HomeView::new_for_test(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.strict_hotkeys = false;
    view.group_by = GroupByMode::Project;
    view.sort_order = SortOrder::Attention;
    view.project_group_collapsed
        .insert("alpha".to_string(), true);
    view.flat_items = view.build_flat_items();

    let alpha_group = view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { name, collapsed, .. } if name == "alpha" && *collapsed))
        .expect("collapsed alpha project group");
    assert!(
        !view
            .flat_items
            .iter()
            .any(|item| matches!(item, Item::Session { id, .. } if id == &alpha_id)),
        "precondition: alpha idle session should be hidden by the collapsed group"
    );
    view.cursor = alpha_group;
    view.update_selected();

    view.handle_key(key(KeyCode::Char('w')), None);

    assert!(
        view.snooze_duration_dialog.is_none(),
        "`w` on a collapsed group must jump, not open the snooze dialog"
    );
    assert_eq!(view.selected_session.as_deref(), Some(alpha_id.as_str()));
    assert!(
        view.flat_items
            .iter()
            .any(|item| matches!(item, Item::Session { id, .. } if id == &alpha_id)),
        "jumping to a hidden idle session should reveal its project group"
    );
}

#[test]
#[serial]
fn test_strict_mode_ctrl_g_opens_group_picker() {
    // The GroupBy binding is Ctrl+G in strict mode and must open the group picker, while
    // bare 'g' still falls into the typing-guard and lands in pending_paste.
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_with_sessions(3);
    env.view.strict_hotkeys = true;
    env.view.group_by = GroupByMode::Manual;

    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.group_picker_dialog.is_some(),
        "Ctrl+G in strict mode should open the group picker"
    );
    assert!(
        env.view.pending_paste.is_none(),
        "Ctrl+G must not leak into the typing-guard catch-all"
    );
    // Down + Enter switches to Project.
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.group_by, GroupByMode::Project);

    env.view.handle_key(key(KeyCode::Char('g')), None);
    assert!(
        env.view.group_picker_dialog.is_none(),
        "bare 'g' in strict mode must NOT open the group picker (typing-guard contract)"
    );
    assert_eq!(
        env.view.group_by,
        GroupByMode::Project,
        "bare 'g' in strict mode must NOT change group-by (typing-guard contract)"
    );
    assert_eq!(
        env.view.pending_paste.as_deref(),
        Some("g"),
        "bare 'g' in strict mode falls through to the typing-guard catch-all"
    );
}

#[test]
#[serial]
fn test_strict_mode_ctrl_t_and_ctrl_n_reach_secondary_actions() {
    // normalize_strict_key used to fold Ctrl+T -> 'T' and Ctrl+N -> 'N', colliding with the
    // Shift+T / Shift+N primary arms and leaving the Ctrl secondary arms (quick-attach
    // terminal, new-from-selection) unreachable. Both chords must keep CTRL.
    let mut env = create_test_env_with_sessions(1);
    env.view.strict_hotkeys = true;
    env.view.cursor = 0;
    env.view.update_selected();

    // Shift+T toggles the view (primary action), no terminal attach.
    assert_eq!(env.view.view_mode, ViewMode::Structured);
    let shift_t = env
        .view
        .handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT), None);
    assert_eq!(env.view.view_mode, ViewMode::Terminal);
    assert!(
        !matches!(shift_t, Some(Action::AttachTerminal(_, _))),
        "Shift+T must toggle view, not attach terminal"
    );
    // Reset to Structured view.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('T'), KeyModifiers::SHIFT), None);
    assert_eq!(env.view.view_mode, ViewMode::Structured);

    // Ctrl+T quick-attaches the paired terminal (secondary action) and must
    // NOT toggle the view.
    let ctrl_t = env.view.handle_key(
        KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        matches!(ctrl_t, Some(Action::AttachTerminal(_, _))),
        "Ctrl+T in strict mode must quick-attach the paired terminal"
    );
    assert_eq!(
        env.view.view_mode,
        ViewMode::Structured,
        "Ctrl+T must not toggle the view"
    );

    // Shift+N opens the plain new-session dialog (no prefill from selection).
    assert!(env.view.new_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.new_dialog.is_some(),
        "Shift+N must open the new-session dialog"
    );
    env.view.new_dialog = None;

    // Ctrl+N opens the new-from-selection dialog. It also routes through
    // open_new_session_dialog, so the dialog opening with CTRL intact is what proves the
    // secondary arm fired.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.new_dialog.is_some(),
        "Ctrl+N in strict mode must open the new-from-selection dialog"
    );
}

#[test]
#[serial]
fn test_strict_mode_ctrl_d_r_p_reach_secondary_actions() {
    // normalize_strict_key used to fold Ctrl+D/R/P to bare 'D'/'R'/'P', colliding with the
    // Shift+letter primary arms, so Ctrl+D fired delete instead of diff, Ctrl+R rename
    // instead of serve, and the projects arm was orphaned. All three must keep CTRL.
    let mut env = create_test_env_with_sessions(1);
    disable_delete_to_trash();
    env.view.strict_hotkeys = true;
    env.view.cursor = 0;
    env.view.update_selected();

    // Shift+D opens the delete confirmation (primary uppercase action).
    assert!(env.view.unified_delete_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.unified_delete_dialog.is_some(),
        "Shift+D must open the delete dialog"
    );
    env.view.unified_delete_dialog = None;

    // Ctrl+D routes to diff, not delete. The test session's path is not a real worktree, so
    // the diff view may fail to open or open empty; either way Ctrl+D must never reach
    // open_delete_for_selected. Clear any takeover it leaves so the next keypress lands.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.unified_delete_dialog.is_none(),
        "Ctrl+D in strict mode must NOT open the delete dialog (it targets diff)"
    );
    env.view.diff_view = None;
    env.view.info_dialog = None;

    // Shift+R opens the rename dialog (primary uppercase action).
    assert!(env.view.rename_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('R'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.rename_dialog.is_some(),
        "Shift+R must open the rename dialog"
    );
    env.view.rename_dialog = None;

    // Ctrl+R routes to the serve arm, NOT rename.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.rename_dialog.is_none(),
        "Ctrl+R in strict mode must NOT open the rename dialog (it targets serve)"
    );
    env.view.info_dialog = None;
    env.view.serve_view = None;

    // P follows the same relocation rule as D/R/T/N, so in strict mode Shift+P opens
    // projects and Ctrl+P opens profiles.
    assert!(env.view.projects_dialog.is_none());
    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT), None);
    assert!(
        env.view.projects_dialog.is_some(),
        "Shift+P in strict mode must open the projects dialog"
    );
    assert!(
        env.view.profile_picker_dialog.is_none(),
        "Shift+P must not open the profile picker"
    );
    env.view.projects_dialog = None;

    // Ctrl+P opens the profile picker, NOT projects.
    assert!(env.view.profile_picker_dialog.is_none());
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.profile_picker_dialog.is_some(),
        "Ctrl+P in strict mode must open the profile picker"
    );
    assert!(
        env.view.projects_dialog.is_none(),
        "Ctrl+P must not open the projects dialog"
    );
}

#[test]
#[serial]
fn test_command_palette_diff_invokes_diff_in_strict_mode() {
    // The palette half of the strict-mode bug: the palette used to synthesize a keypress,
    // so picking "Open diff view" routed through Shift+D and fired delete. Entries now carry
    // an ActionId and run the action directly.
    let mut env = create_test_env_with_sessions(1);
    env.view.strict_hotkeys = true;
    env.view.cursor = 0;
    env.view.update_selected();

    // Open the palette and filter to the diff command.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        None,
    );
    assert!(
        env.view.command_palette.is_some(),
        "Ctrl+K opens the palette"
    );
    for ch in "diff view".chars() {
        env.view.handle_key(key(KeyCode::Char(ch)), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);

    // The diff action ran (opened the diff view, or raised an info dialog if the
    // temp path isn't a real git repo). Crucially, it did NOT delete.
    assert!(
        env.view.unified_delete_dialog.is_none(),
        "palette 'diff' in strict mode must not open the delete dialog"
    );
    assert!(
        env.view.diff_view.is_some() || env.view.info_dialog.is_some(),
        "palette 'diff' in strict mode must attempt to open the diff view"
    );
}

#[test]
#[serial]
fn test_f5_and_e_both_open_restart_dialog() {
    // Pin the equivalence: F5 and `e`/`E` all open the restart dialog, which is what makes
    // the help overlay's "Restart session (also F5)" row honest.
    let mut env = create_test_env_with_sessions(1);
    env.view.cursor = 0;
    env.view.update_selected();

    env.view.handle_key(key(KeyCode::F(5)), None);
    let f5_opened = env.view.restart_dialog.is_some();
    env.view.restart_dialog = None;

    env.view.strict_hotkeys = false;
    env.view.handle_key(key(KeyCode::Char('e')), None);
    let lower_e_opened = env.view.restart_dialog.is_some();
    env.view.restart_dialog = None;

    env.view.strict_hotkeys = true;
    env.view.handle_key(key(KeyCode::Char('E')), None);
    let upper_e_opened = env.view.restart_dialog.is_some();

    assert!(f5_opened, "F5 should open the restart dialog");
    assert!(
        lower_e_opened,
        "non-strict 'e' should open the restart dialog"
    );
    assert!(upper_e_opened, "strict 'E' should open the restart dialog");
}

/// Titles of the sessions rendered under the "work" group header, in list order.
fn work_group_titles(view: &HomeView) -> Vec<&str> {
    let mut titles = Vec::new();
    let mut in_work_group = false;
    for item in &view.flat_items {
        match item {
            Item::Group { name, .. } => in_work_group = name == "work",
            Item::Session { id, .. } => {
                if in_work_group {
                    if let Some(inst) = view.get_instance(id) {
                        titles.push(inst.title.as_str());
                    }
                }
            }
        }
    }
    titles
}

/// Sort order reaches the sessions inside a group: the picker's AZ and ZA entries reorder
/// the work group, and six `o` presses wrap back to Newest and its original order.
#[test]
#[serial]
fn test_o_key_flat_items_follow_sort_order() {
    use crate::session::config::SortOrder;

    for (downs, order, expected) in [
        (4, SortOrder::AZ, ["Apple", "Mango", "Zebra"]),
        (5, SortOrder::ZA, ["Zebra", "Mango", "Apple"]),
    ] {
        let mut env = create_test_env_with_mixed_sessions();
        assert_eq!(env.view.sort_order, SortOrder::Newest);

        env.view.handle_key(key(KeyCode::Char('o')), None);
        for _ in 0..downs {
            env.view.handle_key(key(KeyCode::Down), None);
        }
        env.view.handle_key(key(KeyCode::Enter), None);

        assert_eq!(env.view.sort_order, order);
        assert_eq!(work_group_titles(&env.view), expected);
    }

    // With the picker open a mnemonic applies its order outright, no arrow keys.
    let mut env = create_test_env_with_mixed_sessions();
    for (letter, expected) in [
        ('c', SortOrder::Custom),
        ('t', SortOrder::Attention),
        ('n', SortOrder::Newest),
    ] {
        env.view.handle_key(key(KeyCode::Char('o')), None);
        env.view.handle_key(key(KeyCode::Char(letter)), None);
        assert_eq!(env.view.sort_order, expected, "{letter}");
    }
    assert_eq!(work_group_titles(&env.view), ["Apple", "Mango", "Zebra"]);
}

#[test]
#[serial]
fn test_o_key_clamps_cursor_when_list_shrinks() {
    use crate::session::config::SortOrder;
    use tui_input::Input;

    let mut env = create_test_env_with_mixed_sessions();
    let initial_items = env.view.flat_items.len();

    env.view.cursor = initial_items - 1;
    assert_eq!(env.view.cursor, initial_items - 1);

    // Set up a search query but don't activate search mode
    // (simulates having just exited search mode with matches)
    env.view.search_query = Input::new("work".to_string());
    env.view.update_search();
    let filtered_count = env.view.search_matches.len();
    assert!(filtered_count < initial_items);

    // Open the sort picker and pick Attention (one entry down from Newest).
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Down), None);
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::Attention);

    let valid_max = env.view.flat_items.len().saturating_sub(1);
    assert!(env.view.cursor <= valid_max);
}

/// The unified view loads every profile as flat depth-0 rows with no profile headers; a
/// profile-filtered view loads only its own profile.
#[test]
#[serial]
fn test_all_profiles_view_loads_from_multiple_profiles() {
    let (_temp, _guard) = test_home();
    seed_profile("alpha", &[Instance::new("Alpha Session", "/tmp/a")]);
    seed_profile("beta", &[Instance::new("Beta Session", "/tmp/b")]);

    let mut view = test_view(None);
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    let mut profiles: Vec<&str> = view
        .instances()
        .map(|i| i.source_profile.as_str())
        .collect();
    profiles.sort_unstable();
    assert_eq!(profiles, ["alpha", "beta"]);
    assert_eq!(view.flat_items.len(), 2, "no profile headers");
    assert!(view
        .flat_items
        .iter()
        .all(|item| matches!(item, Item::Session { depth: 0, .. })));

    let view = test_view(Some("alpha"));
    assert_eq!(view.instances().len(), 1);
    assert_eq!(view.instance_at(0).title, "Alpha Session");
    assert_eq!(view.instance_at(0).source_profile, "alpha");
}

/// Ctrl+Down and Ctrl+Up move a session within its group under the Custom sort, and the move
/// survives a rebuild because it is stored on the session rather than recomputed.
#[test]
#[serial]
fn test_ctrl_arrows_move_a_session_under_custom_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    // Set the order directly: `o` cycles from whatever the previous serial test persisted.
    env.view.apply_sort_order(SortOrder::Custom);
    let original: Vec<String> = work_group_titles(&env.view)
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(original.len(), 3);

    // Land the cursor on the first session of the work group.
    let work_ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .filter(|id| {
            env.view
                .get_instance(id)
                .map(|s| s.group_path == "work")
                .unwrap_or(false)
        })
        .collect();
    let first = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if id == &work_ids[0]))
        .expect("a session under work");
    env.view.cursor = first;
    env.view.update_selected();

    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);
    let moved: Vec<String> = work_group_titles(&env.view)
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(
        moved,
        [
            original[1].clone(),
            original[0].clone(),
            original[2].clone()
        ]
    );

    env.view.rebuild_flat_items();
    assert_eq!(
        work_group_titles(&env.view),
        moved,
        "order is persisted, not recomputed"
    );

    env.view
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), None);
    assert_eq!(work_group_titles(&env.view), original);
}

/// Outside the Custom sort the same keys leave the list alone: a computed order would
/// discard the move on the next rebuild, so the action explains itself instead.
#[test]
#[serial]
fn test_ctrl_arrows_do_not_reorder_under_a_computed_sort() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    env.view.apply_sort_order(SortOrder::Newest);
    let before: Vec<String> = work_group_titles(&env.view)
        .into_iter()
        .map(str::to_string)
        .collect();

    let work_ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .filter(|id| {
            env.view
                .get_instance(id)
                .map(|s| s.group_path == "work")
                .unwrap_or(false)
        })
        .collect();
    let first = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if id == &work_ids[0]))
        .expect("a session under work");
    env.view.cursor = first;
    env.view.update_selected();

    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    assert_eq!(work_group_titles(&env.view), before);
    assert!(
        env.view.status_flash.is_some(),
        "the user is told why nothing moved"
    );
}

/// Alt+Down and Alt+Up walk the sessions the theme paints as attention-worthy: `Running`
/// (green) and the ones that just finished a turn (Idle inside the decay window). A session
/// that went idle long ago is skipped.
#[test]
#[serial]
fn test_alt_arrows_jump_between_just_finished_sessions() {
    use crate::session::Status;
    use chrono::{Duration as ChronoDuration, Utc};

    let mut env = create_test_env_with_mixed_sessions();
    env.view.idle_decay_window = std::time::Duration::from_secs(30 * 60);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert!(ids.len() >= 3);

    // One finished a minute ago and one is working; the one between them went idle yesterday.
    let fresh = Utc::now() - ChronoDuration::minutes(1);
    let stale = Utc::now() - ChronoDuration::days(1);
    for (idx, status, entered) in [
        (0, Status::Idle, Some(fresh)),
        (1, Status::Idle, Some(stale)),
        (2, Status::Running, None),
    ] {
        env.view
            .apply_user_action(&ids[idx], |inst| {
                inst.status = status;
                inst.idle_entered_at = entered;
            })
            .unwrap();
    }
    env.view.rebuild_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();

    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), None);
    assert_eq!(
        env.view.selected_session,
        Some(ids[2].clone()),
        "walked past the stale row onto the working one"
    );

    env.view
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT), None);
    assert_ne!(
        env.view.selected_session,
        Some(ids[1].clone()),
        "the session idle since yesterday is skipped"
    );
}

/// A session at the edge of its group keeps going into the neighbouring group, landing
/// against the boundary it crossed rather than at a far end.
#[test]
#[serial]
fn test_ctrl_arrows_carry_a_session_into_the_next_group() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    env.view.apply_sort_order(SortOrder::Custom);

    let work_ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .filter(|id| {
            env.view
                .get_instance(id)
                .map(|s| s.group_path == "work")
                .unwrap_or(false)
        })
        .collect();
    assert!(work_ids.len() >= 2, "the fixture groups several sessions");
    let top = work_ids[0].clone();

    let cursor = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if id == &top))
        .expect("the work group's first session");
    env.view.cursor = cursor;
    env.view.update_selected();

    // Above "work" sits the ungrouped bucket, so moving up leaves the group.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), None);
    assert_eq!(
        env.view.get_instance(&top).map(|i| i.group_path.clone()),
        Some(String::new()),
        "the session left its group"
    );

    // And back again, landing at the top of the group it re-enters.
    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);
    assert_eq!(
        env.view.get_instance(&top).map(|i| i.group_path.clone()),
        Some("work".to_string())
    );
    assert_eq!(
        env.view.get_instance(&top).and_then(|i| i.sort_index),
        Some(0),
        "it re-enters at the boundary it crossed"
    );
}

/// Ctrl+Down on a group header moves that group past its sibling, and the new order is what
/// the list shows on the next rebuild.
#[test]
#[serial]
fn test_ctrl_arrows_move_a_group_among_its_siblings() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "alpha"),
        instance_in("b1", "/tmp/b1", "beta"),
        instance_in("c1", "/tmp/c1", "gamma"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);

    let group_names = |view: &HomeView| -> Vec<String> {
        view.flat_items
            .iter()
            .filter_map(|i| match i {
                Item::Group { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect()
    };
    let before = group_names(&env.view);
    assert_eq!(before, ["alpha", "beta", "gamma"], "fixture order");

    let header = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "alpha"))
        .expect("alpha header");
    env.view.cursor = header;
    env.view.update_selected();
    assert_eq!(env.view.selected_group.as_deref(), Some("alpha"));

    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);
    assert_eq!(group_names(&env.view), ["beta", "alpha", "gamma"]);

    env.view.rebuild_flat_items();
    assert_eq!(
        group_names(&env.view),
        ["beta", "alpha", "gamma"],
        "persisted"
    );
}

/// The last row of the last group has nowhere to go: Archived and Trash are sinks a session
/// reaches by being archived or trashed, never by being moved into them.
#[test]
#[serial]
fn test_ctrl_down_stops_at_the_last_group() {
    use crate::session::config::SortOrder;

    let mut archived = instance_in("gone", "/tmp/gone", "alpha");
    archived.archive();
    let instances = [
        instance_in("a1", "/tmp/a1", "alpha"),
        instance_in("b1", "/tmp/b1", "beta"),
        archived,
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);
    assert!(
        env.view
            .flat_items
            .iter()
            .any(|i| matches!(i, Item::Group { name, .. } if name.contains("Archived"))),
        "the fixture shows an Archived section to move into"
    );

    let last = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .filter_map(|(idx, i)| match i {
            Item::Session { id, .. } => env
                .view
                .get_instance(id)
                .filter(|inst| inst.title == "b1")
                .map(|_| (idx, id.clone())),
            _ => None,
        })
        .next()
        .expect("b1 row");
    env.view.cursor = last.0;
    env.view.update_selected();

    for _ in 0..3 {
        env.view
            .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);
    }

    assert_eq!(
        env.view.get_instance(&last.1).map(|i| i.group_path.clone()),
        Some("beta".to_string()),
        "stayed in the last real group"
    );
}

/// Inside a session the relay swallows every key until the exit chord, except the jump
/// keys: they mean "take me elsewhere", so they leave live mode and land on the next
/// working or just-finished session.
#[test]
#[serial]
fn test_alt_arrows_leave_live_send_before_jumping() {
    use crate::session::Status;
    use chrono::{Duration as ChronoDuration, Utc};

    let mut env = create_test_env_with_mixed_sessions();
    env.view.idle_decay_window = std::time::Duration::from_secs(30 * 60);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert!(ids.len() >= 3);

    env.view
        .apply_user_action(&ids[2], |inst| {
            inst.status = Status::Idle;
            inst.idle_entered_at = Some(Utc::now() - ChronoDuration::minutes(1));
        })
        .unwrap();
    env.view.rebuild_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    env.view.live_send = Some(live_send_state(&ids[0], "relaying", "aoe_test_live_jump"));

    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::ALT), None);

    assert!(env.view.live_send.is_none(), "left the relay");
    assert_eq!(
        env.view.selected_session,
        Some(ids[2].clone()),
        "and landed on the just-finished session"
    );
}

/// Project and Org headers are derived from repo paths, and their rows carry a rewritten
/// `group_path` for display. A move there would persist a manual membership the list is not
/// showing, so it is refused and the stored groups stay put.
#[test]
#[serial]
fn test_row_moves_are_refused_outside_manual_grouping() {
    use crate::session::config::{GroupByMode, SortOrder};

    let instances = [
        instance_in("a1", "/tmp/alpha", "work"),
        instance_in("b1", "/tmp/beta", "work"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);
    env.view.group_by = GroupByMode::Project;
    env.view.rebuild_flat_items();

    let first = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .find_map(|(idx, i)| match i {
            Item::Session { id, .. } => Some((idx, id.clone())),
            _ => None,
        })
        .expect("a session row");
    let membership = |view: &HomeView| -> Vec<String> {
        let (instances, _) = Storage::open_unwatched("test")
            .unwrap()
            .load_with_groups()
            .unwrap();
        let _ = view;
        instances
            .iter()
            .map(|i| format!("{}={}", i.title, i.group_path))
            .collect()
    };
    let before = membership(&env.view);
    env.view.cursor = first.0;
    env.view.update_selected();

    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    assert_eq!(
        membership(&env.view),
        before,
        "manual group membership untouched"
    );
    assert!(env.view.status_flash.is_some(), "the refusal is explained");
}

/// A group that exists only because a session names it has no `groups.json` row yet. Moving
/// it must still persist the new order, or the next load rebuilds the original one.
#[test]
#[serial]
fn test_moving_a_group_without_a_stored_row_persists() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "alpha"),
        instance_in("b1", "/tmp/b1", "beta"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    // The seeder writes a row per group; drop them so the groups exist only through the
    // sessions that name them, which is what the group PATCH endpoint leaves behind.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|_, groups| {
            groups.clear();
            Ok(())
        })
        .unwrap();
    env.view.apply_sort_order(SortOrder::Custom);

    let header = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "alpha"))
        .expect("alpha header");
    env.view.cursor = header;
    env.view.update_selected();
    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    let (_, stored) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let paths: Vec<String> = stored.into_iter().map(|g| g.path).collect();
    assert_eq!(paths, ["beta", "alpha"], "permutation reached the store");
}

/// A peer can delete a group while this view holds a tree that still lists it. The move must
/// be computed from what the store holds, or writing the stale tree back resurrects it.
#[test]
#[serial]
fn test_moving_a_group_does_not_resurrect_a_peer_deleted_one() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "alpha"),
        instance_in("b1", "/tmp/b1", "beta"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);

    // A peer removes "gamma" (present on disk and in this view's tree) behind our back.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|_, groups| {
            groups.push(crate::session::Group {
                name: "gamma".to_string(),
                path: "gamma".to_string(),
                collapsed: false,
                archived_at: None,
                children: Vec::new(),
            });
            Ok(())
        })
        .unwrap();
    env.view.reload_storage_only().unwrap();
    Storage::open_unwatched("test")
        .unwrap()
        .update(|_, groups| {
            groups.retain(|g| g.path != "gamma");
            Ok(())
        })
        .unwrap();

    let header = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { name, .. } if name == "alpha"))
        .expect("alpha header");
    env.view.cursor = header;
    env.view.update_selected();
    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    let (_, stored) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let paths: Vec<String> = stored.into_iter().map(|g| g.path).collect();
    assert!(
        !paths.iter().any(|p| p == "gamma"),
        "the peer's deletion stands: {paths:?}"
    );
    assert_eq!(paths, ["beta", "alpha"], "and the move still landed");
}

/// Two profiles can hold the same group path. Restoring the cursor by path alone lands it on
/// whichever comes first, leaving `selected_group_profile` naming the other profile, so the
/// next move would reorder a profile the cursor is not on.
#[test]
#[serial]
fn test_cursor_restore_keeps_the_group_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    crate::session::create_profile("first").unwrap();
    crate::session::create_profile("second").unwrap();
    seed_profile("first", &[instance_in("a1", "/tmp/a1", "work")]);
    seed_profile("second", &[instance_in("b1", "/tmp/b1", "work")]);

    let mut view = HomeView::new_for_test(
        None,
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    // Deliberately the header that is not first: a path-only restore would land on the
    // other profile's row, which is what this pins.
    let target = view
        .flat_items
        .iter()
        .position(|i| {
            matches!(i, Item::Group { path, profile, .. }
            if path == "work" && profile.as_deref() == Some("first"))
        })
        .expect("first profile's work header");
    assert!(target > 0, "the other profile's header comes earlier");
    view.cursor = target;
    view.update_selected();
    assert_eq!(view.selected_group_profile.as_deref(), Some("first"));

    view.rebuild_flat_items_keeping_cursor();

    assert!(
        matches!(view.flat_items.get(view.cursor), Some(Item::Group { path, profile, .. })
            if path == "work" && profile.as_deref() == Some("first")),
        "cursor stayed on the first profile's header"
    );
}

/// A single-profile view leaves a header's profile unset. An empty or collapsed neighbour is
/// still a destination: skipping it would carry the session past it into the group beyond.
#[test]
#[serial]
fn test_cross_group_move_lands_in_an_empty_neighbour() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "alpha"),
        instance_in("c1", "/tmp/c1", "gamma"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    // "beta" sits between them with no sessions of its own.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|_, groups| {
            groups.insert(
                1,
                crate::session::Group {
                    name: "beta".to_string(),
                    path: "beta".to_string(),
                    collapsed: true,
                    archived_at: None,
                    children: Vec::new(),
                },
            );
            Ok(())
        })
        .unwrap();
    env.view.reload_storage_only().unwrap();
    env.view.apply_sort_order(SortOrder::Custom);

    let moving = env
        .view
        .flat_items
        .iter()
        .find_map(|i| match i {
            Item::Session { id, .. } => env
                .view
                .get_instance(id)
                .filter(|inst| inst.title == "a1")
                .map(|_| id.clone()),
            _ => None,
        })
        .expect("a1 row");
    let at = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if *id == moving))
        .expect("row index");
    env.view.cursor = at;
    env.view.update_selected();

    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    assert_eq!(
        env.view.get_instance(&moving).map(|i| i.group_path.clone()),
        Some("beta".to_string()),
        "landed in the adjacent group, not past it"
    );
    // The destination was collapsed; the row it received has to stay visible, or the cursor
    // cannot follow it and the next move acts on a hidden session.
    assert!(
        env.view
            .flat_items
            .iter()
            .any(|i| matches!(i, Item::Session { id, .. } if *id == moving)),
        "the moved row is on screen"
    );
    assert_eq!(env.view.selected_session.as_deref(), Some(moving.as_str()));
    assert!(
        matches!(env.view.flat_items.get(env.view.cursor), Some(Item::Session { id, .. }) if *id == moving),
        "and the cursor sits on it"
    );
}

/// A peer reorders the same group between this view's read and its write. Computing the
/// permutation from the locked rows keeps the indices a permutation of 0..n; computing it
/// from a stale snapshot leaves two rows sharing one.
#[test]
#[serial]
fn test_a_peer_swap_does_not_leave_duplicate_indices() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "work"),
        instance_in("b1", "/tmp/b1", "work"),
        instance_in("c1", "/tmp/c1", "work"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);

    let by_title = |view: &HomeView, title: &str| -> String {
        view.flat_items
            .iter()
            .find_map(|i| match i {
                Item::Session { id, .. } => view
                    .get_instance(id)
                    .filter(|inst| inst.title == title)
                    .map(|_| id.clone()),
                _ => None,
            })
            .expect("session by title")
    };
    let (a, b, c) = (
        by_title(&env.view, "a1"),
        by_title(&env.view, "b1"),
        by_title(&env.view, "c1"),
    );

    // Seed 0/1/2, which is what the view has read.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            for (id, index) in [(&a, 0u32), (&b, 1), (&c, 2)] {
                if let Some(row) = rows.iter_mut().find(|r| r.id == *id) {
                    row.sort_index = Some(index);
                }
            }
            Ok(())
        })
        .unwrap();
    env.view.reload_storage_only().unwrap();

    // A peer swaps b and c behind this view's back; the view still believes 0/1/2.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            for (id, index) in [(&b, 2u32), (&c, 1)] {
                if let Some(row) = rows.iter_mut().find(|r| r.id == *id) {
                    row.sort_index = Some(index);
                }
            }
            Ok(())
        })
        .unwrap();

    let at = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if *id == a))
        .expect("a1 row");
    env.view.cursor = at;
    env.view.update_selected();
    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    let (rows, _) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let mut indices: Vec<u32> = rows.iter().filter_map(|r| r.sort_index).collect();
    indices.sort_unstable();
    assert_eq!(
        indices,
        vec![0, 1, 2],
        "indices stay a permutation: {indices:?}"
    );
}

/// Session id of the row titled `title`.
fn custom_order_id_of(view: &HomeView, title: &str) -> String {
    view.flat_items
        .iter()
        .find_map(|i| match i {
            Item::Session { id, .. } => view
                .get_instance(id)
                .filter(|inst| inst.title == title)
                .map(|_| id.clone()),
            _ => None,
        })
        .expect("session by title")
}

/// A peer deletes the group the cursor is about to move into, between this view's read and
/// its write. The destination is checked against the locked rows, so the move is dropped:
/// writing the membership blind would make the next tree rebuild synthesize the group back
/// from its new member and undo the deletion.
#[test]
#[serial]
fn test_a_move_does_not_resurrect_a_group_a_peer_deleted() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "aaa"),
        instance_in("b1", "/tmp/b1", "bbb"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);
    let a = custom_order_id_of(&env.view, "a1");
    let b = custom_order_id_of(&env.view, "b1");

    // The peer empties bbb and removes the group; this view still draws both.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, groups| {
            rows.retain(|r| r.id != b);
            groups.retain(|g| g.path != "bbb");
            Ok(())
        })
        .unwrap();
    assert!(
        env.view.get_instance(&b).is_some(),
        "the deleted row is still on this view's screen"
    );

    let at = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if *id == a))
        .expect("a1 row");
    env.view.cursor = at;
    env.view.update_selected();
    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    let (rows, groups) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.id == a)
            .map(|r| r.group_path.clone()),
        Some("aaa".to_string()),
        "the row stays where it was"
    );
    assert!(
        !groups.iter().any(|g| g.path == "bbb"),
        "and the deleted group is not written back"
    );
    assert!(
        env.view.get_instance(&b).is_none(),
        "the list is refreshed instead, dropping the peer's deleted row"
    );
    assert!(env.view.status_flash.is_some(), "the refresh is explained");
}

/// A peer moves the cursor's session to another group first. The anchor no longer sits where
/// the list drew it, so the move is dropped and the list refreshed: taken as an edge instead,
/// it would push the row into whichever group the stale list showed next.
#[test]
#[serial]
fn test_a_move_is_dropped_when_a_peer_regroups_the_row() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "work"),
        instance_in("x1", "/tmp/x1", "work"),
        instance_in("c1", "/tmp/c1", "zzz"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);
    let a = custom_order_id_of(&env.view, "a1");
    let x = custom_order_id_of(&env.view, "x1");

    // A known order for the work group, which the view then reads.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            for (id, index) in [(&a, 0u32), (&x, 1)] {
                if let Some(row) = rows.iter_mut().find(|r| r.id == *id) {
                    row.sort_index = Some(index);
                }
            }
            Ok(())
        })
        .unwrap();
    env.view.reload_storage_only().unwrap();

    // The peer moves a1 out of work behind this view's back.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            if let Some(row) = rows.iter_mut().find(|r| r.id == a) {
                row.group_path = "other".to_string();
            }
            Ok(())
        })
        .unwrap();

    let at = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if *id == a))
        .expect("a1 row");
    env.view.cursor = at;
    env.view.update_selected();
    env.view
        .handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::CONTROL), None);

    let (rows, _) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.id == a)
            .map(|r| r.group_path.clone()),
        Some("other".to_string()),
        "the peer's grouping stands"
    );
    assert_eq!(
        env.view
            .get_instance(&a)
            .map(|i| i.group_path.clone())
            .as_deref(),
        Some("other"),
        "and the list now shows it there"
    );
}

/// The move is stored before the destination group is expanded, so a failed expansion write
/// still leaves the row in its new group on disk. The list is rebuilt either way: returning
/// early would leave the cursor on a row the store places elsewhere.
#[test]
#[serial]
fn test_a_failed_expansion_still_settles_the_list() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "aaa"),
        instance_in("b1", "/tmp/b1", "bbb"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);
    let a = custom_order_id_of(&env.view, "a1");

    // What a committed cross-group move leaves behind, before the expansion write is tried.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            if let Some(row) = rows.iter_mut().find(|r| r.id == a) {
                row.group_path = "bbb".to_string();
            }
            Ok(())
        })
        .unwrap();
    if let Some(row) = env.view.instances.get_mut(&a) {
        row.group_path = "bbb".to_string();
    }

    env.view.after_committed_cross_group_move(
        &a,
        "bbb",
        Err(anyhow::anyhow!("groups.json write failed")),
    );

    let header = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { path, .. } if path == "bbb"))
        .expect("bbb header");
    let row = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id, .. } if *id == a))
        .expect("a1 row");
    assert!(
        row > header,
        "the list is rebuilt around the stored move: a1 at {row}, bbb header at {header}"
    );
    assert!(env.view.status_flash.is_some(), "the failure is explained");
}

/// The boundary half of a move runs in a second transaction, after the first has reported
/// the row against the edge of its group. A peer that regroups the row in between must not
/// have its change overwritten by a membership decided from the earlier read.
#[test]
#[serial]
fn test_a_boundary_move_rechecks_the_anchor_against_the_store() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "work"),
        instance_in("c1", "/tmp/c1", "zzz"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);
    let a = custom_order_id_of(&env.view, "a1");

    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, _| {
            if let Some(row) = rows.iter_mut().find(|r| r.id == a) {
                row.group_path = "other".to_string();
            }
            Ok(())
        })
        .unwrap();

    env.view
        .move_session_across_groups(&a, 1)
        .expect("the boundary move is dropped, not failed");

    let (rows, _) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.id == a)
            .map(|r| r.group_path.clone()),
        Some("other".to_string()),
        "the peer's grouping stands"
    );
}

fn stored_group_order(profile: &str) -> Vec<String> {
    Storage::open_unwatched(profile)
        .unwrap()
        .load_with_groups()
        .unwrap()
        .1
        .into_iter()
        .map(|group| group.path)
        .collect()
}

/// A unified view over a single profile draws its headers unqualified, and a stored group with
/// no sessions has no member to infer an owner from. Without the sole store resolved
/// explicitly the move had no profile to write to and did nothing at all.
#[test]
#[serial]
fn test_an_empty_group_moves_in_a_unified_single_profile_view() {
    use crate::session::config::SortOrder;

    let (temp, guard) = test_home();
    let (_temp, _guard) = (temp, guard);
    seed_profile("test", &[]);
    Storage::open_unwatched("test")
        .unwrap()
        .update(|_rows, groups| {
            *groups = ["alpha", "beta"]
                .into_iter()
                .map(|path| Group {
                    name: path.to_string(),
                    path: path.to_string(),
                    collapsed: false,
                    archived_at: None,
                    children: Vec::new(),
                })
                .collect();
            Ok(())
        })
        .unwrap();

    let mut view = test_view(None);
    assert!(view.active_profile.is_none(), "a unified view");
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.apply_sort_order(SortOrder::Custom);
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let at = view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Group { path, .. } if path == "beta"))
        .expect("the beta header is drawn");
    view.cursor = at;
    view.update_selected();
    assert_eq!(view.selected_group.as_deref(), Some("beta"));
    assert!(
        view.selected_group_profile.is_none(),
        "an empty group's header carries no profile, which is the case under test"
    );

    view.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), None);

    assert_eq!(
        stored_group_order("test"),
        ["beta", "alpha"],
        "the move reached the only store there is"
    );
}

/// When the destination stays collapsed because its expansion write failed, the moved row is
/// not drawn. Leaving the selection on it would point the cursor at a session the list does
/// not show, so it falls back to the header the row went under.
#[test]
#[serial]
fn test_a_hidden_destination_moves_the_selection_to_its_header() {
    use crate::session::config::SortOrder;

    let instances = [
        instance_in("a1", "/tmp/a1", "aaa"),
        instance_in("b1", "/tmp/b1", "bbb"),
    ];
    let mut env = seeded_env(test_home(), &instances, true);
    env.view.apply_sort_order(SortOrder::Custom);
    let a = custom_order_id_of(&env.view, "a1");

    // What a committed cross-group move leaves behind when the destination is folded and the
    // write that would have opened it failed.
    Storage::open_unwatched("test")
        .unwrap()
        .update(|rows, groups| {
            if let Some(row) = rows.iter_mut().find(|r| r.id == a) {
                row.group_path = "bbb".to_string();
            }
            for group in groups.iter_mut() {
                if group.path == "bbb" {
                    group.collapsed = true;
                }
            }
            Ok(())
        })
        .unwrap();
    if let Some(row) = env.view.instances.get_mut(&a) {
        row.group_path = "bbb".to_string();
    }
    if let Some(tree) = env.view.group_trees.get_mut("test") {
        tree.set_collapsed("bbb", true);
    }
    env.view.selected_session = Some(a.clone());

    env.view.after_committed_cross_group_move(
        &a,
        "bbb",
        Err(anyhow::anyhow!("groups.json write failed")),
    );

    assert!(
        !env.view
            .flat_items
            .iter()
            .any(|i| matches!(i, Item::Session { id, .. } if *id == a)),
        "the row is inside the folded group, so it is not drawn"
    );
    assert_eq!(
        env.view.selected_group.as_deref(),
        Some("bbb"),
        "the selection follows it as far as the header"
    );
    assert!(
        matches!(env.view.flat_items.get(env.view.cursor), Some(Item::Group { path, .. }) if path == "bbb"),
        "and the cursor sits on that header"
    );
    assert!(env.view.selected_session.is_none());
}

/// Ctrl+Alt+arrow is nobody's chord here, so it must not act as the jump keys do: the jump is
/// Alt alone, and treating Ctrl+Alt as one would move the cursor off a session whose relay was
/// meant to receive the keystroke.
#[test]
#[serial]
fn test_ctrl_alt_arrows_do_not_jump_out_of_live_send() {
    use crate::session::Status;
    use chrono::{Duration as ChronoDuration, Utc};

    let mut env = create_test_env_with_mixed_sessions();
    env.view.idle_decay_window = std::time::Duration::from_secs(30 * 60);
    let ids: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Session { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert!(ids.len() >= 3);

    // The same fixture the Alt+Down jump lands on, so a jump here would be unmistakable.
    env.view
        .apply_user_action(&ids[2], |inst| {
            inst.status = Status::Idle;
            inst.idle_entered_at = Some(Utc::now() - ChronoDuration::minutes(1));
        })
        .unwrap();
    env.view.rebuild_flat_items();
    env.view.cursor = 0;
    env.view.update_selected();
    let before = env.view.selected_session.clone();
    env.view.live_send = Some(live_send_state(&ids[0], "relaying", "aoe_test_ctrl_alt"));

    env.view.handle_key(
        KeyEvent::new(KeyCode::Down, KeyModifiers::ALT | KeyModifiers::CONTROL),
        None,
    );

    assert_eq!(
        env.view.selected_session, before,
        "Ctrl+Alt+Down is not the jump chord, so the cursor stays put"
    );
    assert_ne!(
        env.view.selected_session,
        Some(ids[2].clone()),
        "and in particular it did not land on the just-finished session"
    );
}

/// A session alone in `parent/child`, with no stored group rows at all: the tree still draws
/// `parent`, because it draws every ancestor of a member's path, so Ctrl+Up has to be able to
/// land the row there. Counting only exact members took `parent` for a deleted group, refreshed
/// into the same tree, and the row never moved however often the key was pressed.
#[test]
#[serial]
fn test_a_session_moves_up_into_an_implicit_parent_group() {
    use crate::session::config::SortOrder;

    let instances = [instance_in("c1", "/tmp/c1", "parent/child")];
    let mut env = seeded_env(test_home(), &instances, true);
    Storage::open_unwatched("test")
        .unwrap()
        .update(|_rows, groups| {
            groups.clear();
            Ok(())
        })
        .unwrap();
    env.view.reload_storage_only().unwrap();
    env.view.apply_sort_order(SortOrder::Custom);
    let stored_groups = || {
        Storage::open_unwatched("test")
            .unwrap()
            .load_with_groups()
            .unwrap()
            .1
    };
    assert!(
        stored_groups().is_empty(),
        "no stored group rows, which is the case under test"
    );
    let headers: Vec<String> = env
        .view
        .flat_items
        .iter()
        .filter_map(|i| match i {
            Item::Group { path, .. } => Some(path.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        headers,
        ["parent", "parent/child"],
        "both headers are drawn from the one member's path"
    );

    let id = custom_order_id_of(&env.view, "c1");
    env.view.cursor = env
        .view
        .flat_items
        .iter()
        .position(|i| matches!(i, Item::Session { id: row, .. } if *row == id))
        .expect("c1 row");
    env.view.update_selected();
    env.view
        .handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL), None);

    let (rows, _) = Storage::open_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert_eq!(
        rows.iter()
            .find(|r| r.id == id)
            .map(|r| r.group_path.clone()),
        Some("parent".to_string()),
        "the row lands in the implicit parent"
    );
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(id.as_str()),
        "the selection follows it"
    );
    assert!(
        matches!(env.view.flat_items.get(env.view.cursor), Some(Item::Session { id: row, .. }) if *row == id),
        "and it is drawn under the cursor"
    );

    env.view.reload_storage_only().unwrap();
    assert_eq!(
        env.view
            .get_instance(&id)
            .map(|i| i.group_path.clone())
            .as_deref(),
        Some("parent"),
        "the move survives a reload from disk"
    );
}

/// Two profiles can hold groups with the same path. When the expansion write fails and the
/// selection falls back to the destination's header, that has to be the header in the moving
/// row's own profile: matching the path alone picks whichever profile is drawn first.
#[test]
#[serial]
fn test_a_hidden_destination_falls_back_to_the_header_in_its_own_profile() {
    use crate::session::config::SortOrder;

    let (temp, guard) = test_home();
    let (_temp, _guard) = (temp, guard);
    seed_profile("alpha", &[instance_in("a1", "/tmp/a1", "bbb")]);
    seed_profile(
        "beta",
        &[
            instance_in("b1", "/tmp/b1", "aaa"),
            instance_in("b2", "/tmp/b2", "bbb"),
        ],
    );
    let mut view = test_view(None);
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.apply_sort_order(SortOrder::Custom);
    view.flat_items = view.build_flat_items();
    view.update_selected();

    let b1 = custom_order_id_of(&view, "b1");
    let first_bbb = view.flat_items.iter().find_map(|i| match i {
        Item::Group { path, profile, .. } if path == "bbb" => Some(profile.clone()),
        _ => None,
    });
    assert_eq!(
        first_bbb,
        Some(Some("alpha".to_string())),
        "alpha's bbb is drawn first, so matching the path alone would land there"
    );

    // What a committed move of b1 into beta's bbb leaves behind when the write that would have
    // opened the folded group failed.
    Storage::open_unwatched("beta")
        .unwrap()
        .update(|rows, groups| {
            if let Some(row) = rows.iter_mut().find(|r| r.id == b1) {
                row.group_path = "bbb".to_string();
            }
            for group in groups.iter_mut() {
                if group.path == "bbb" {
                    group.collapsed = true;
                }
            }
            Ok(())
        })
        .unwrap();
    if let Some(row) = view.instances.get_mut(&b1) {
        row.group_path = "bbb".to_string();
    }
    if let Some(tree) = view.group_trees.get_mut("beta") {
        tree.set_collapsed("bbb", true);
    }
    view.selected_session = Some(b1.clone());

    view.after_committed_cross_group_move(
        &b1,
        "bbb",
        Err(anyhow::anyhow!("groups.json write failed")),
    );

    assert_eq!(view.selected_group.as_deref(), Some("bbb"));
    assert_eq!(
        view.selected_group_profile.as_deref(),
        Some("beta"),
        "the header the selection lands on is in the moving row's own profile"
    );
}
