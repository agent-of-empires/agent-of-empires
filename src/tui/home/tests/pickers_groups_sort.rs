//! Pickers, settings entry, group management, layout, and sort.

use super::*;

#[test]
#[serial]
fn test_uppercase_p_picker_esc_closes() {
    let env = create_test_env_empty();
    let mut view = env.view;

    view.handle_key(key(KeyCode::Char('P')), None);
    assert!(view.profile_picker_dialog.is_some());

    view.handle_key(key(KeyCode::Esc), None);
    assert!(view.profile_picker_dialog.is_none());
}

#[test]
#[serial]
fn test_uppercase_p_picker_switch_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    crate::session::create_profile("first").unwrap();
    crate::session::create_profile("second").unwrap();

    let _storage = Storage::new_unwatched("first").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("first".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Open picker
    view.handle_key(key(KeyCode::Char('P')), None);
    assert!(view.profile_picker_dialog.is_some());

    // In filtered mode, "all" is at top, then "first", "second", "test"
    // Navigate down to reach "second" and select it
    view.handle_key(key(KeyCode::Down), None);
    view.handle_key(key(KeyCode::Down), None);
    view.handle_key(key(KeyCode::Down), None);
    let action = view.handle_key(key(KeyCode::Enter), None);
    // Profile switch is handled internally, no Action returned
    assert_eq!(action, None);
    assert_eq!(view.active_profile, Some("second".to_string()));
    assert!(view.profile_picker_dialog.is_none());
}

#[test]
#[serial]
fn test_t_toggles_view_mode() {
    let env = create_test_env_empty();
    let mut view = env.view;

    assert_eq!(view.view_mode, ViewMode::Structured);

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Structured);
}

#[test]
#[serial]
fn switching_view_retargets_capture_worker_pane() {
    // The preview's off-thread capture worker follows the displayed pane:
    // switching agent <-> terminal must resolve to different tmux sessions
    // so `sync_preview_capture_worker` respawns the worker against the new
    // pane (instead of the old agent-only behavior). Regression guard for
    // the responsiveness fix that moved every preview's `tmux capture-pane`
    // off the render thread.
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

#[test]
#[serial]
fn test_enter_returns_attach_terminal_in_terminal_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // In Structured view, Enter returns AttachSession
    let action = view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachSession(_))));

    // Switch to Terminal view
    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    // In Terminal view, Enter returns AttachTerminal
    let action = view.handle_key(key(KeyCode::Enter), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
}

#[test]
#[serial]
fn test_shift_t_attaches_terminal_from_structured_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // Should be in Structured view by default
    assert_eq!(view.view_mode, ViewMode::Structured);

    // Shift+T should return AttachTerminal without switching view mode
    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
    assert_eq!(view.view_mode, ViewMode::Structured);
}

#[test]
#[serial]
fn test_shift_t_attaches_terminal_from_terminal_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // Switch to Terminal view
    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    // Shift+T should also work from Terminal view
    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(matches!(action, Some(Action::AttachTerminal(_, _))));
}

#[test]
#[serial]
fn test_shift_t_noop_with_no_sessions() {
    let env = create_test_env_empty();
    let mut view = env.view;

    let action = view.handle_key(key(KeyCode::Char('T')), None);
    assert!(action.is_none());
}

#[test]
#[serial]
fn test_d_shows_info_dialog_in_terminal_view() {
    let env = create_test_env_with_sessions(1);
    let mut view = env.view;

    // Switch to Terminal view
    view.handle_key(key(KeyCode::Char('t')), None);
    assert_eq!(view.view_mode, ViewMode::Terminal);

    // Press 'd' - should show info dialog, not delete dialog
    assert!(view.info_dialog.is_none());
    view.handle_key(key(KeyCode::Char('d')), None);
    assert!(view.info_dialog.is_some());
    assert!(view.unified_delete_dialog.is_none());
}

#[test]
#[serial]
fn test_has_dialog_includes_info_dialog() {
    let env = create_test_env_empty();
    let mut view = env.view;

    assert!(!view.has_dialog());

    view.info_dialog = Some(InfoDialog::new("Test", "Test message"));
    assert!(view.has_dialog());
}

#[test]
#[serial]
fn test_has_dialog_includes_settings_view() {
    use crate::tui::settings::SettingsView;

    let env = create_test_env_empty();
    let mut view = env.view;

    assert!(!view.has_dialog());

    view.settings_view = Some(SettingsView::new("test", None).unwrap());
    assert!(view.has_dialog());
}

#[test]
#[serial]
fn test_s_opens_settings_view() {
    let mut env = create_test_env_empty();
    assert!(env.view.settings_view.is_none());
    env.view.handle_key(key(KeyCode::Char('s')), None);
    assert!(env.view.settings_view.is_some());
}

/// Trashing and restoring a session through the view's own actions keeps the
/// group header count in step with the rows, since both are rebuilt from the
/// same predicate. Guards against the count and the visible rows drifting.
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
fn test_group_has_managed_worktrees() {
    use crate::session::WorktreeInfo;
    use chrono::Utc;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.worktree_info = Some(WorktreeInfo {
        branch: "feature-branch".to_string(),
        main_repo_path: "/tmp/main".to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    let mut inst2 = Instance::new("other-session", "/tmp/other");
    inst2.group_path = "other".to_string();

    {
        let xs: Vec<Instance> = vec![inst1, inst2];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert!(view.group_has_managed_worktrees("work", "work/", None));
    assert!(!view.group_has_managed_worktrees("other", "other/", None));
}

#[test]
#[serial]
fn test_group_has_containers() {
    use crate::session::SandboxInfo;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: None,
        image: "ubuntu:latest".to_string(),
        container_name: "test-container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });

    let mut inst2 = Instance::new("other-session", "/tmp/other");
    inst2.group_path = "other".to_string();

    {
        let xs: Vec<Instance> = vec![inst1, inst2];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert!(view.group_has_containers("work", "work/", None));
    assert!(!view.group_has_containers("other", "other/", None));
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

/// Locks #1868: bulk archive persists synchronously even though tmux
/// teardown runs off-thread. Real tmux state asserted in
/// `tests/e2e/archive_restore.rs`.
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

/// In project group-by mode, archiving a project header archives every live
/// session that maps to that repo, even though their stored `group_path`
/// values differ from the synthetic project name.
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
    let mut view = HomeView::new(
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

/// The group-level prompt opens a confirmation carrying the `archive_group`
/// action and counts only the active members, and no-ops without a prompt when
/// the group has nothing left to archive.
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
    let mut view = HomeView::new(
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
fn test_delete_group_with_sessions_respects_worktree_option() {
    use crate::session::WorktreeInfo;
    use crate::tui::dialogs::GroupDeleteOptions;
    use chrono::Utc;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.worktree_info = Some(WorktreeInfo {
        branch: "feature".to_string(),
        main_repo_path: "/tmp/main".to_string(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: None,
    });

    {
        let xs: Vec<Instance> = vec![inst1];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Select the work group
    view.cursor = 0;
    view.update_selected();
    assert!(view.selected_group.is_some());

    // Delete with worktrees option enabled
    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: true,
        delete_branches: false,
        delete_containers: false,
        force_delete_worktrees: false,
    };
    view.delete_group_with_sessions(&options).unwrap();

    // We can't easily verify the deletion request was sent with the right flags
    // without mocking, but we can verify the group was deleted
    assert!(!view.group_trees.get("test").unwrap().group_exists("work"));
}

#[test]
#[serial]
fn test_delete_group_with_sessions_respects_container_option() {
    use crate::session::SandboxInfo;
    use crate::tui::dialogs::GroupDeleteOptions;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("work-session", "/tmp/work");
    inst1.group_path = "work".to_string();
    inst1.sandbox_info = Some(SandboxInfo {
        enabled: true,
        container_id: None,
        image: "ubuntu:latest".to_string(),
        container_name: "test-container".to_string(),
        extra_env: None,
        custom_instruction: None,
        before_start_env: Vec::new(),
        container_workdir: None,
    });

    {
        let xs: Vec<Instance> = vec![inst1];
        storage
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // Select the work group
    view.cursor = 0;
    view.update_selected();
    assert!(view.selected_group.is_some());

    // Delete with containers option enabled
    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: false,
        delete_branches: false,
        delete_containers: true,
        force_delete_worktrees: false,
    };
    view.delete_group_with_sessions(&options).unwrap();

    // Verify the group was deleted
    assert!(!view.group_trees.get("test").unwrap().group_exists("work"));
}

#[test]
#[serial]
fn test_delete_group_includes_nested_groups() {
    use crate::tui::dialogs::GroupDeleteOptions;

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

    // Verify nested group exists
    assert!(env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work/projects"));

    // Delete the group with all sessions
    let options = GroupDeleteOptions {
        delete_sessions: true,
        delete_worktrees: false,
        delete_branches: false,
        delete_containers: false,
        force_delete_worktrees: false,
    };
    env.view.delete_group_with_sessions(&options).unwrap();

    // Verify both parent and nested groups are removed
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work"));
    assert!(!env
        .view
        .group_trees
        .get("test")
        .unwrap()
        .group_exists("work/projects"));
}

#[test]
#[serial]
fn test_groups_field_stays_in_sync_with_storage() {
    let mut env = create_test_env_with_group_sessions();

    // Get initial group count
    let initial_group_count = env.view.all_groups().len();
    assert!(initial_group_count > 0);

    // Select and delete the work group
    for (i, item) in env.view.flat_items.iter().enumerate() {
        if let Item::Group { path, .. } = item {
            if path == "work" {
                env.view.cursor = i;
                env.view.update_selected();
                break;
            }
        }
    }

    env.view.delete_selected_group().unwrap();

    // After deletion, groups field should be smaller
    assert!(env.view.all_groups().len() < initial_group_count);

    // Reload from storage and verify groups match
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

#[test]
#[serial]
fn test_group_collapsed_state_persists_across_reload() {
    let mut env = create_test_env_with_groups();

    // Find a group and verify it starts expanded
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("should have a group");

    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(!collapsed, "group should start expanded");
    }

    // Move cursor to group and collapse it with Enter
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);

    // Verify it's collapsed
    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx] {
        assert!(*collapsed, "group should be collapsed after Enter");
    }

    // Reload (simulates the 5-second periodic refresh)
    env.view.reload().unwrap();

    // Find the group again (index may change after reload)
    let group_idx_after = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { .. }))
        .expect("should still have a group");

    // Verify it's still collapsed after reload
    if let Item::Group { collapsed, .. } = &env.view.flat_items[group_idx_after] {
        assert!(*collapsed, "group should remain collapsed after reload");
    }
}

#[test]
#[serial]
fn test_group_collapsed_state_saved_to_storage() {
    use crate::session::GroupTree;

    let mut env = create_test_env_with_groups();

    // Find a group
    let group_path = env
        .view
        .flat_items
        .iter()
        .find_map(|item| {
            if let Item::Group { path, .. } = item {
                Some(path.clone())
            } else {
                None
            }
        })
        .expect("should have a group");

    // Move cursor to group and collapse it
    let group_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Group { path, .. } if path == &group_path))
        .unwrap();
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);

    // Load fresh from storage to verify persistence
    let (_, groups) = env
        .view
        .storages
        .get("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let fresh_tree =
        GroupTree::new_with_groups(&env.view.instances().cloned().collect::<Vec<_>>(), &groups);
    let all_groups = fresh_tree.get_all_groups();

    let saved_group = all_groups
        .iter()
        .find(|g| g.path == group_path)
        .expect("group should exist in storage");

    assert!(
        saved_group.collapsed,
        "collapsed state should be persisted to storage"
    );
}

/// Project-mode folder collapse must survive a restart. Unlike group mode
/// (persisted on the per-profile GroupTree), project folders are auto-derived
/// and have no group record, so their collapse state is written to
/// `app_state.project_group_collapsed`. Regression for collapsed project
/// folders re-expanding on relaunch.
#[test]
#[serial]
fn test_project_group_collapsed_state_persists_to_config() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // Find a project folder header and confirm it starts expanded.
    let (group_idx, group_path) = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .find_map(|(idx, item)| match item {
            Item::Group {
                path, collapsed, ..
            } => {
                assert!(!collapsed, "project folder should start expanded");
                Some((idx, path.clone()))
            }
            _ => None,
        })
        .expect("project mode should have a folder header");

    // Collapse it via Enter, which routes through toggle_group_collapsed.
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);

    // The collapsed path must be persisted to the on-disk config.
    let config = crate::session::config::load_config()
        .unwrap()
        .expect("config should exist after collapse");
    assert!(
        config
            .app_state
            .project_group_collapsed
            .contains(&group_path),
        "collapsed project folder path should be persisted to app_state"
    );

    // A freshly constructed HomeView (simulating relaunch) must restore it.
    let fresh = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert_eq!(
        fresh.project_group_collapsed.get(&group_path).copied(),
        Some(true),
        "relaunched HomeView should restore the collapsed project folder"
    );
}

/// A collapse entry for a project that no longer exists must be pruned on save
/// so the persisted set can't grow without bound as projects come and go. A
/// still-live folder collapsed in the same session must survive.
#[test]
#[serial]
fn test_project_group_collapsed_prunes_stale_paths() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Project;
    env.view.flat_items = env.view.build_flat_items();

    // A real folder the user collapsed this session.
    let live_path = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            Item::Group { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("project mode should have a folder header");

    env.view
        .project_group_collapsed
        .insert(live_path.clone(), true);
    // A stale entry for a project that isn't part of this session at all.
    env.view
        .project_group_collapsed
        .insert("/repos/deleted-ghost".to_string(), true);

    env.view.save_project_group_collapsed();

    let config = crate::session::config::load_config()
        .unwrap()
        .expect("config should exist after save");
    let saved = &config.app_state.project_group_collapsed;
    assert!(
        saved.contains(&live_path),
        "a live collapsed folder must be persisted"
    );
    assert!(
        !saved.iter().any(|p| p == "/repos/deleted-ghost"),
        "a collapse entry for a nonexistent project must be pruned"
    );
}

/// Org-mode counterpart of `test_project_group_collapsed_state_persists_to_config`:
/// org folder collapse state has no group record either (headers are derived
/// from each session's resolved remote owner), so it must round-trip through
/// `app_state.org_group_collapsed` the same way.
#[test]
#[serial]
fn test_org_group_collapsed_state_persists_to_config() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Org;
    env.view.flat_items = env.view.build_flat_items();

    // Find an org folder header and confirm it starts expanded.
    let (group_idx, group_path) = env
        .view
        .flat_items
        .iter()
        .enumerate()
        .find_map(|(idx, item)| match item {
            Item::Group {
                path, collapsed, ..
            } => {
                assert!(!collapsed, "org folder should start expanded");
                Some((idx, path.clone()))
            }
            _ => None,
        })
        .expect("org mode should have a folder header");

    // Collapse it via Enter, which routes through toggle_group_collapsed.
    env.view.cursor = group_idx;
    env.view.update_selected();
    env.view.handle_key(key(KeyCode::Enter), None);

    // The collapsed path must be persisted to the on-disk config.
    let config = crate::session::config::load_config()
        .unwrap()
        .expect("config should exist after collapse");
    assert!(
        config.app_state.org_group_collapsed.contains(&group_path),
        "collapsed org folder path should be persisted to app_state"
    );

    // A freshly constructed HomeView (simulating relaunch) must restore it.
    let fresh = HomeView::new(
        Some("test".to_string()),
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert_eq!(
        fresh.org_group_collapsed.get(&group_path).copied(),
        Some(true),
        "relaunched HomeView should restore the collapsed org folder"
    );
}

/// Org-mode counterpart of `test_project_group_collapsed_prunes_stale_paths`.
#[test]
#[serial]
fn test_org_group_collapsed_prunes_stale_paths() {
    use crate::session::config::GroupByMode;

    let mut env = create_test_env_two_projects_mixed_attention();
    env.view.group_by = GroupByMode::Org;
    env.view.flat_items = env.view.build_flat_items();

    // A real folder the user collapsed this session.
    let live_path = env
        .view
        .flat_items
        .iter()
        .find_map(|item| match item {
            Item::Group { path, .. } => Some(path.clone()),
            _ => None,
        })
        .expect("org mode should have a folder header");

    env.view.org_group_collapsed.insert(live_path.clone(), true);
    // A stale entry for an org that isn't part of this session at all.
    env.view
        .org_group_collapsed
        .insert("stale-org".to_string(), true);

    env.view.save_org_group_collapsed();

    let config = crate::session::config::load_config()
        .unwrap()
        .expect("config should exist after save");
    let saved = &config.app_state.org_group_collapsed;
    assert!(
        saved.contains(&live_path),
        "a live collapsed folder must be persisted"
    );
    assert!(
        !saved.iter().any(|p| p == "stale-org"),
        "a collapse entry for a nonexistent org must be pruned"
    );
}

#[test]
#[serial]
fn test_list_width_default() {
    let env = create_test_env_empty();
    assert_eq!(env.view.list_width, 35);
}

#[test]
#[serial]
fn test_shrink_list() {
    let mut env = create_test_env_empty();
    env.view.shrink_list();
    assert_eq!(env.view.list_width, 30);
}

#[test]
#[serial]
fn test_grow_list() {
    let mut env = create_test_env_empty();
    env.view.grow_list();
    assert_eq!(env.view.list_width, 40);
}

#[test]
#[serial]
fn test_shrink_list_clamps_at_minimum() {
    let mut env = create_test_env_empty();
    env.view.list_width = 12;
    env.view.shrink_list();
    assert_eq!(env.view.list_width, 10);
    env.view.shrink_list();
    assert_eq!(env.view.list_width, 10);
}

#[test]
#[serial]
fn test_grow_list_clamps_at_maximum() {
    let mut env = create_test_env_empty();
    env.view.list_width = 78;
    env.view.grow_list();
    assert_eq!(env.view.list_width, 80);
    env.view.grow_list();
    assert_eq!(env.view.list_width, 80);
}

#[test]
#[serial]
fn test_lt_shrinks_list() {
    let mut env = create_test_env_empty();
    assert_eq!(env.view.list_width, 35);
    env.view.handle_key(key(KeyCode::Char('<')), None);
    assert_eq!(env.view.list_width, 30);
}

#[test]
#[serial]
fn test_gt_grows_list() {
    let mut env = create_test_env_empty();
    assert_eq!(env.view.list_width, 35);
    env.view.handle_key(key(KeyCode::Char('>')), None);
    assert_eq!(env.view.list_width, 40);
}

#[test]
#[serial]
fn test_sort_order_defaults_to_newest() {
    use crate::session::config::SortOrder;

    let env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);
}

/// The picker must not offer a repo the session already has: the attach would
/// be rejected as a duplicate, so offering it is offering a guaranteed failure.
/// With no registry entries there is nothing to offer, and the dialog says so
/// rather than rendering an empty list.
#[test]
#[serial]
fn add_project_picker_opens_and_excludes_repos_already_on_the_session() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

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

    // Esc closes without attaching.
    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.attach_project_dialog.is_none());
    assert!(
        env.view
            .get_instance(&id)
            .is_some_and(|i| i.all_repos().is_empty()),
        "cancelling must not attach anything"
    );
}

/// Attaching bounces the worker and creates a worktree, so the picker must
/// refuse the same lifecycle states every sibling mutator refuses. The context
/// menu offers the row unconditionally, so this gate is the only thing stopping
/// an archived or mid-turn session from being attached to.
#[test]
#[serial]
fn add_project_picker_refuses_shelved_and_mid_turn_sessions() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    // `Waiting` and `Starting` are turns in flight too, which is why the gate
    // reuses `Status::blocks_worktree_edit()` rather than naming `Running` alone:
    // SIGTERMing a `Waiting` worker throws away a pending approval.
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

    // Idle and unshelved: the picker opens.
    env.view.mutate_instance(&id, |inst| inst.unarchive());
    env.view.info_dialog = None;
    env.view.open_add_project_for_selected();
    assert!(
        env.view.attach_project_dialog.is_some(),
        "an idle, unshelved session must be attachable"
    );
}

/// The attach runs on a background poller, so the dispatch must return without
/// touching git: `git worktree add` plus an optional fetch and submodule init on
/// the render thread froze the UI for the whole attach. A second dispatch for the
/// same session is refused, because it would race the first one's worktree
/// creation and its worker bounce.
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

/// The completion path clears the marker and replaces the progress dialog, for
/// both outcomes. Without the clear, one failed attach would leave the session
/// permanently unattachable.
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

/// A scratch session has no repo of its own, so there is nothing for an
/// attached one to widen and deletion drops its whole directory. The picker
/// refuses it outright rather than opening on a list where every choice would be
/// rejected by `attach_project::plan`.
#[test]
#[serial]
fn add_project_picker_refuses_a_scratch_session() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    env.view.mutate_instance(&id, |inst| inst.scratch = true);
    env.view.open_add_project_for_selected();
    assert!(
        env.view.attach_project_dialog.is_none(),
        "a scratch session has no repo to attach to"
    );
    assert!(
        env.view.info_dialog.is_some(),
        "the refusal must be visible, not a silent no-op"
    );

    // The same session stops being scratch and becomes attachable, so the
    // refusal is keyed on the flag rather than on some other property of the row.
    env.view.mutate_instance(&id, |inst| inst.scratch = false);
    env.view.info_dialog = None;
    env.view.open_add_project_for_selected();
    assert!(env.view.attach_project_dialog.is_some());
}

/// The picker is a modal, so it has to register in the overlay predicates that
/// gate scroll, right-click, footer clicks, drag start, and paste-burst routing.
/// Missing from them, the wheel moved the cursor underneath the open modal and
/// right-click stacked a second context menu on top of it.
#[test]
#[serial]
fn add_project_picker_registers_as_an_overlay() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());

    assert!(!env.view.has_dialog(), "no dialog open yet");

    env.view.open_add_project_for_selected();
    assert!(
        env.view.attach_project_dialog.is_some(),
        "picker should be open"
    );
    assert!(
        env.view.has_dialog(),
        "an open picker must count as a dialog, or list keyboard actions fire behind it"
    );
    assert!(
        env.view.has_non_live_send_overlay(),
        "an open picker must count as an overlay, or scroll and right-click reach the list under it"
    );

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(
        !env.view.has_dialog(),
        "closing the picker clears the dialog"
    );
    assert!(
        !env.view.has_non_live_send_overlay(),
        "closing the picker clears the non-live overlay"
    );
}

#[test]
#[serial]
fn test_o_key_opens_sort_picker() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // 'o' opens the picker; the current sort is unchanged until the user
    // confirms a selection.
    env.view.handle_key(key(KeyCode::Char('o')), None);
    assert!(env.view.sort_picker_dialog.is_some());
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Walk to AZ (Newest -> Attention -> LastActivity -> Oldest -> AZ) and
    // confirm.
    for _ in 0..4 {
        env.view.handle_key(key(KeyCode::Down), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert!(env.view.sort_picker_dialog.is_none());
    assert_eq!(env.view.sort_order, SortOrder::AZ);
}

#[test]
#[serial]
fn test_shift_o_opens_sort_picker_in_strict_mode() {
    // Regression guard: the SortPicker binding lists Shift+O (Char('O')) for
    // strict mode, so it must resolve to the sort picker rather than falling
    // through to the typing-guard (capture_letter_to_compose).
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

    // Sort order is unchanged because no selection was confirmed.
    assert_eq!(env.view.sort_order, SortOrder::Newest);
    // Sanity: message dialog must NOT have been opened as a side effect.
    assert!(env.view.send_message_dialog.is_none());
}

#[test]
#[serial]
fn test_bare_lowercase_o_does_not_cycle_sort_in_strict_mode() {
    // Regression guard (2026-04-22): in strict_hotkeys mode, plain lowercase 'o'
    // MUST NOT cycle sort; it must fall through to the typing-guard catch-all
    // (message dialog) per the "no destructive lowercase" rule. Only Shift+O
    // (Char('O')) and Ctrl+O should change sort order in strict mode.
    //
    // The previous implementation collapsed the two sort arms into a single
    // unguarded `Char('o') => cycle`, which fired for bare 'o' too, breaking
    // the contract and silently changing the user's sort order whenever they
    // tried to type 'o' as text input.
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    env.view.strict_hotkeys = true;
    let initial = env.view.sort_order;
    assert_eq!(initial, SortOrder::Newest);

    env.view
        .handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), None);

    assert_eq!(
        env.view.sort_order, initial,
        "bare 'o' in strict mode must NOT cycle sort; expected it to stay at Newest"
    );
}

#[test]
#[serial]
fn test_strict_mode_h_collapses_group() {
    // Regression guard: the help overlay lists "h/←" for Collapse group in
    // strict mode. Bare lowercase `h` must walk through the dispatch and
    // collapse the cursor's group, mirroring `l`/Right for expand. Without
    // the explicit `Char('h')` arm next to `KeyCode::Left`, `h` would fall
    // into the strict-mode typing-guard catch-all and the advertised
    // navigation hotkey would silently open the compose dialog.
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
    // Snooze is Attention-mode-only: in Attention sort `h` toggles snooze on
    // the cursor's session and the group below the cursor stays expanded;
    // in every other sort mode the snooze arm declines, control falls
    // through to the unconditional `Left | Char('h')` collapse handler,
    // and the group collapses. Before the gating, snooze always caught
    // first in non-strict mode regardless of sort, which silently mutated
    // persisted state for users who weren't using Attention sort.
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_groups();
    env.view.strict_hotkeys = false;

    // Attention sort flattens groups out, so seed a cursor-on-session
    // scenario and assert that `h` opens the snooze duration dialog
    // (the actual snooze fires when the user picks a duration).
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
    // Regression for #1524: in non-strict Attention sort, `w` must jump to the
    // next waiting/idle session (the #796 behavior) instead of snoozing the
    // cursor's session. Snooze lives on `h`/`H`; `w` is navigation. Previously
    // the snooze arm shadowed the jump arm in exactly the sort users triage in,
    // so `w` never felt like a navigation key.
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

    let mut view = HomeView::new(
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
    // Regression guard: the GroupBy binding is Ctrl+G in strict mode. It must
    // open the group picker, while bare 'g' continues to fall into the
    // typing-guard catch-all (it lands in pending_paste).
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
    // Regression guard (2026-05-29): in strict_hotkeys mode, normalize_strict_key
    // used to fold Ctrl+T -> 'T' and Ctrl+N -> 'N' (modifier stripped), which
    // collided with the Shift+T / Shift+N primary arms (toggle view, plain new
    // session) and left the Ctrl+T / Ctrl+N secondary arms (quick-attach
    // terminal, new-from-selection) as unreachable dead code. Both chords must
    // keep CTRL so the secondary arms fire.
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

    // Ctrl+N opens the new-from-selection dialog (secondary action). It also
    // routes through open_new_session_dialog, so assert it reaches the arm by
    // confirming the dialog opens with CTRL intact rather than being swallowed.
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
    // Regression guard (2026-05-29): normalize_strict_key used to fold
    // Ctrl+D/Ctrl+R/Ctrl+P to bare 'D'/'R'/'P', which collided with the
    // Shift+letter primary arms. In strict mode Shift+D=delete, Shift+R=rename,
    // Shift+P=profiles, so the folds made Ctrl+D fire delete (not diff), Ctrl+R
    // fire rename (not serve), and orphaned the diff/serve/projects arms. All
    // three Ctrl chords must keep CTRL so their secondary arms fire.
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

    // Ctrl+D routes to the diff arm, NOT delete. The test session's path is not
    // a real git worktree so the diff view may fail to open (info dialog) or
    // open empty; either way the regression is that Ctrl+D must never reach
    // open_delete_for_selected. Clear any takeover the diff arm leaves behind so
    // it doesn't swallow the next keypress.
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

    // P follows the same relocation rule as D/R/T/N: the bare-`p` (primary)
    // action -> Shift+P, the Shift+P (secondary) action -> Ctrl+P. So in strict
    // mode Shift+P opens projects and Ctrl+P opens profiles.
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
    // Regression guard for the palette half of the strict-mode bug: the palette
    // used to synthesize a keypress, so picking "Open diff view" in strict mode
    // routed through Shift+D and fired DELETE instead. Palette entries now carry
    // an ActionId and run the action directly, so the mode can't matter.
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
    // Pin the equivalence: F5 and `e`/`E` all open the restart dialog. The
    // help overlay collapses them onto one row as "Restart session (also
    // F5)", which is only honest if both bindings keep hitting the same
    // dispatch (open_restart_dialog).
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

#[test]
#[serial]
fn test_ctrl_o_key_opens_sort_picker() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Ctrl+O opens the same modal picker. Pressing it on its own does not
    // change the current sort.
    env.view.handle_key(
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
        None,
    );
    assert!(env.view.sort_picker_dialog.is_some());
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    env.view.handle_key(key(KeyCode::Esc), None);
    assert!(env.view.sort_picker_dialog.is_none());
    assert_eq!(env.view.sort_order, SortOrder::Newest);
}

#[test]
#[serial]
fn test_o_key_flat_items_sorted_az() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    // Open the sort picker and pick AZ.
    env.view.handle_key(key(KeyCode::Char('o')), None);
    for _ in 0..4 {
        env.view.handle_key(key(KeyCode::Down), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::AZ);

    let mut session_titles: Vec<_> = Vec::new();
    let mut in_work_group = false;
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => {
                in_work_group = name == "work";
            }
            Item::Session { id, .. } => {
                if in_work_group {
                    if let Some(inst) = env.view.get_instance(id) {
                        session_titles.push(inst.title.as_str());
                    }
                }
            }
        }
    }

    assert_eq!(session_titles, vec!["Apple", "Mango", "Zebra"]);
}

#[test]
#[serial]
fn test_o_key_flat_items_sorted_za() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();

    // Open the sort picker and pick ZA (5 entries down from Newest).
    env.view.handle_key(key(KeyCode::Char('o')), None);
    for _ in 0..5 {
        env.view.handle_key(key(KeyCode::Down), None);
    }
    env.view.handle_key(key(KeyCode::Enter), None);
    assert_eq!(env.view.sort_order, SortOrder::ZA);

    let mut session_titles: Vec<_> = Vec::new();
    let mut in_work_group = false;
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => {
                in_work_group = name == "work";
            }
            Item::Session { id, .. } => {
                if in_work_group {
                    if let Some(inst) = env.view.get_instance(id) {
                        session_titles.push(inst.title.as_str());
                    }
                }
            }
        }
    }

    assert_eq!(session_titles, vec!["Zebra", "Mango", "Apple"]);
}

#[test]
#[serial]
fn test_o_key_flat_items_newest_preserves_insertion_order() {
    use crate::session::config::SortOrder;

    let mut env = create_test_env_with_mixed_sessions();

    // Press 'o' six times to wrap back to Newest
    // (Newest -> Attention -> LastActivity -> Oldest -> AZ -> ZA -> Newest)
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    env.view.handle_key(key(KeyCode::Char('o')), None);
    assert_eq!(env.view.sort_order, SortOrder::Newest);

    let mut session_titles: Vec<_> = Vec::new();
    let mut in_work_group = false;
    for item in &env.view.flat_items {
        match item {
            Item::Group { name, .. } => {
                in_work_group = name == "work";
            }
            Item::Session { id, .. } => {
                if in_work_group {
                    if let Some(inst) = env.view.get_instance(id) {
                        session_titles.push(inst.title.as_str());
                    }
                }
            }
        }
    }

    assert_eq!(session_titles, vec!["Apple", "Mango", "Zebra"]);
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

#[test]
#[serial]
fn test_all_profiles_view_loads_from_multiple_profiles() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("Alpha Session", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("Beta Session", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert_eq!(view.instances().len(), 2);
    let profiles: Vec<&str> = view
        .instances()
        .map(|i| i.source_profile.as_str())
        .collect();
    assert!(profiles.contains(&"alpha"));
    assert!(profiles.contains(&"beta"));
}

#[test]
#[serial]
fn test_filtered_view_loads_single_profile() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("Alpha Session", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("Beta Session", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(
        Some("alpha".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    assert_eq!(view.instances().len(), 1);
    assert_eq!(view.instance_at(0).title, "Alpha Session");
    assert_eq!(view.instance_at(0).source_profile, "alpha");
}

#[test]
#[serial]
fn test_all_profiles_view_has_no_profile_headers() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("A1", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("B1", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // All items should be sessions (no profile headers)
    let session_count = view
        .flat_items
        .iter()
        .filter(|i| matches!(i, Item::Session { .. }))
        .count();
    assert_eq!(session_count, 2);
    assert_eq!(view.flat_items.len(), 2);
}

#[test]
#[serial]
fn test_all_profiles_view_shows_all_sessions_flat() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);

    let storage_a = Storage::new_unwatched("alpha").unwrap();
    {
        let xs = vec![Instance::new("A1", "/tmp/a")];
        storage_a
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let storage_b = Storage::new_unwatched("beta").unwrap();
    {
        let xs = vec![Instance::new("B1", "/tmp/b")];
        storage_b
            .update(|i, g| {
                *i = xs.to_vec();
                *g = GroupTree::new_with_groups(&xs, &[]).get_all_groups();
                Ok(())
            })
            .unwrap();
    }

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    // All sessions from all profiles should be visible at depth 0
    for item in &view.flat_items {
        if let Item::Session { depth, .. } = item {
            assert_eq!(*depth, 0, "sessions in all view should be at depth 0");
        }
    }
}
