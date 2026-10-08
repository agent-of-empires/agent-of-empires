//! Fork, rename, and dialog focus behavior.

use super::*;

#[test]
#[serial]
fn fork_from_selection_seeds_terminal_fork_and_inherits_parent_context() {
    let mut env = create_test_env_empty();
    let mut inst = observed_fork_parent("claude");
    inst.project_path = "/tmp/repo-worktrees/feature".into();
    inst.agent_session_binding
        .as_mut()
        .unwrap()
        .execution
        .as_mut()
        .unwrap()
        .cwd = inst.project_path.clone().into();
    inst.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature".into(),
        main_repo_path: "/tmp/repo".into(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("fork opens the new-session dialog");
    let seed = dialog.fork_seed().cloned().expect("fork seed present");
    match seed {
        crate::session::ForkSeed::Terminal {
            parent,
            child_session_id,
            ..
        } => {
            assert_eq!(parent.session_id, "parent-1111-2222-3333-444444444444");
            assert_ne!(child_session_id, "parent-1111-2222-3333-444444444444");
            assert!(crate::session::capture::is_valid_session_id(
                &child_session_id
            ));
        }
        other => panic!("expected Terminal fork seed, got {other:?}"),
    }
    assert_eq!(dialog.path_value(), "/tmp/repo-worktrees/feature");
}

/// A launch that pre-pins a child id still records the execution it resolved,
/// so the Fork row shows before any conversation is captured.
#[test]
#[serial]
fn fork_row_offers_a_preallocated_parent() {
    let mut env = create_test_env_empty();
    let mut inst = observed_fork_parent("claude");
    inst.agent_session_binding.as_mut().unwrap().provenance =
        crate::session::ConversationProvenance::Preallocated;
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id.clone());

    assert!(
        env.view.session_can_fork(&id),
        "a preallocated parent records its launch execution, so the row must show"
    );
}

/// A recorded conversation AoE cannot fork is refused in two ways, and the
/// dialog carries the shared wording: a preallocated id names no conversation to
/// qualify, while a binding that never qualified, or one a degraded launch
/// dropped, names a conversation to re-assert.
#[test]
#[serial]
fn fork_from_selection_reports_why_the_conversation_cannot_be_forked() {
    let recorded = "parent-1111-2222-3333-444444444444".to_string();
    let cases = [
        (
            crate::session::ConversationProvenance::Preallocated,
            crate::session::ForkDenied::UnqualifiedParent {
                preallocated: true,
                recorded: recorded.clone(),
            },
        ),
        (
            crate::session::ConversationProvenance::Unknown,
            crate::session::ForkDenied::UnqualifiedParent {
                preallocated: false,
                recorded: recorded.clone(),
            },
        ),
    ];
    for (provenance, denied) in cases {
        let mut env = create_test_env_empty();
        let mut inst = observed_fork_parent("claude");
        inst.agent_session_binding.as_mut().unwrap().provenance = provenance;
        let title = inst.title.clone();
        let id = inst.id.clone();
        let profile = inst.effective_profile();
        env.view.add_instance(inst);
        env.view.selected_session = Some(id.clone());

        env.view.open_fork_from_selection();

        assert!(
            env.view.new_dialog.is_none(),
            "an unqualified parent must not open a fork dialog"
        );
        let dialog = env.view.info_dialog.as_ref().expect("info dialog");
        assert_eq!(dialog.title(), "Conversation not qualified");
        assert_eq!(dialog.message(), denied.user_message(&title, &id, &profile));
    }
}

/// `set-session-id` opens only the store its profile names, so a remedy for a
/// parent living in a non-default profile has to name that profile: run
/// against the default it would qualify nothing.
#[test]
#[serial]
fn the_qualification_remedy_names_the_profile_the_parent_lives_in() {
    let mut env = create_test_env_empty();
    let mut inst = observed_fork_parent("claude");
    inst.source_profile = "client work".into();
    inst.agent_session_binding.as_mut().unwrap().provenance =
        crate::session::ConversationProvenance::Unknown;
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id.clone());

    env.view.open_fork_from_selection();

    let message = &env
        .view
        .info_dialog
        .as_ref()
        .expect("an unqualified parent is refused with a dialog")
        .message()
        .to_string();
    let command = message
        .split_once('`')
        .and_then(|(_, rest)| rest.split_once('`'))
        .map_or_else(
            || panic!("one quoted remedy in: {message}"),
            |(span, _)| span,
        );
    assert_eq!(
        shell_words::split(command).expect("the remedy tokenizes"),
        [
            "aoe",
            "-p",
            "client work",
            "session",
            "set-session-id",
            id.as_str(),
            "parent-1111-2222-3333-444444444444",
        ]
    );
}

/// Unforkable parents get an explanatory info dialog instead of the fork form: a resume-only
/// terminal agent, a structured parent with no captured ACP session, and a structured parent
/// whose agent has no fork strategy even with an ACP id (the capability gate runs first,
/// mirroring the REST create guard and the web `acp_can_fork` projection).
#[test]
#[serial]
fn fork_denied_for_unforkable_parents_shows_info() {
    let structured = |tool: &str, acp_session_id: Option<&str>| {
        let mut inst = Instance::new("parent", "/tmp/repo");
        inst.source_profile = "test".to_string();
        inst.tool = tool.into();
        inst.view = crate::session::View::Structured;
        inst.acp_session_id = acp_session_id.map(Into::into);
        inst
    };
    let cases = [
        observed_fork_parent("gemini"),
        structured("claude", None),
        structured("aoe-agent", Some("acp-parent-1234")),
    ];
    for inst in cases {
        let mut env = create_test_env_empty();
        let tool = inst.tool.clone();
        let id = inst.id.clone();
        env.view.add_instance(inst);
        env.view.selected_session = Some(id);

        env.view.open_fork_from_selection();

        assert!(env.view.new_dialog.is_none(), "{tool}: no fork dialog");
        assert!(env.view.info_dialog.is_some(), "{tool}: info dialog shown");
    }
}

/// The fork seed forks the parent's agent, so the dialog must open preselected
/// on that agent rather than the configured default. A Codex parent forking
/// while the default tool is claude must land on codex, not claude (otherwise
/// the dialog's tool and the seed disagree).
#[test]
#[serial]
fn fork_from_selection_preselects_parent_tool() {
    let mut env = create_test_env_empty();
    env.view
        .set_available_tools(AvailableTools::with_tools(&["claude", "codex"]));
    let inst = observed_fork_parent("codex");
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("fork opens the new-session dialog");
    assert_eq!(
        dialog.selected_tool(),
        "codex",
        "fork dialog must preselect the parent's agent so it matches the seed"
    );
}

/// A structured (ACP) parent forks via the ACP `session/fork` handshake, so the
/// seed must be `Structured` carrying the parent's captured ACP session id, not
/// a terminal resume-with-fork-flag seed.
#[test]
#[serial]
fn fork_from_selection_structured_parent_seeds_structured_fork() {
    let mut env = create_test_env_empty();
    let mut inst = Instance::new("parent", "/tmp/repo");
    inst.source_profile = "test".to_string();
    inst.tool = "claude".into();
    inst.view = crate::session::View::Structured;
    inst.acp_session_id = Some("acp-parent-9999".into());
    let id = inst.id.clone();
    env.view.add_instance(inst);
    env.view.selected_session = Some(id);

    env.view.open_fork_from_selection();

    let dialog = env
        .view
        .new_dialog
        .as_ref()
        .expect("fork opens the new-session dialog for a structured parent");
    let seed = dialog.fork_seed().cloned().expect("fork seed present");
    assert_eq!(
        seed,
        crate::session::ForkSeed::Structured {
            parent_acp_session_id: "acp-parent-9999".into(),
        },
        "a structured parent must seed a structured fork from its ACP session id"
    );
}

/// Context-menu Snooze mirrors the Attention-gated `h` key: on an active session it opens
/// the duration picker, on a snoozed one it submits the wake to the daemon.
#[test]
#[serial]
fn test_session_context_menu_snooze_toggle() {
    use crate::session::config::SortOrder;
    use crate::tui::dialogs::ContextMenuAction;

    let mut env = create_test_env_with_groups();

    let session_idx = env
        .view
        .flat_items
        .iter()
        .position(|item| matches!(item, Item::Session { .. }))
        .expect("setup should produce a session");
    env.view.cursor = session_idx;
    env.view.update_selected();
    let id = env
        .view
        .selected_session
        .clone()
        .expect("a session should be selected");

    // Active row: the menu opens the duration picker, exactly like the `h` key.
    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);
    env.view
        .dispatch_context_menu_action(ContextMenuAction::ToggleSnooze);
    assert!(
        env.view.snooze_duration_dialog.is_some(),
        "context-menu Snooze on an active session must open the duration picker"
    );

    // Snoozed row: the toggle is a daemon mutation, so the row only moves once
    // the canonical snapshot lands.
    env.view.snooze_duration_dialog = None;
    env.view
        .mutate_instance(&id, |instance| instance.snooze(60));
    assert!(
        env.view.instances.get(&id).is_some_and(|i| i.is_snoozed()),
        "session should be snoozed before the toggle"
    );
    env.view
        .dispatch_context_menu_action(ContextMenuAction::ToggleSnooze);
    assert!(
        env.view.snooze_duration_dialog.is_none(),
        "waking a snoozed session must not open the duration picker"
    );
    assert!(
        env.view.instances.get(&id).is_some_and(|i| i.is_snoozed()),
        "an offline action must not alter the snooze"
    );
}

/// `N` prefills the new-session dialog from the selected session: a worktree row borrows its
/// main repo path, an ungrouped row its own path with no group.
#[test]
#[serial]
fn test_shift_n_prefills_from_selected_session() {
    // Rows are only ever built for the active profile
    // (`cloned_instances_in_active_view`), so a fixture row has to carry that
    // profile like the ones `observed_fork_parent` seeds: an instance with an
    // empty `source_profile` is held by the view but never reaches the list.
    let mut worktree = Instance::new("worktree-session", "/tmp/repo-worktrees/feature-branch");
    worktree.source_profile = "test".to_string();
    worktree.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature-branch".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let mut ungrouped = Instance::new("ungrouped", "/tmp/u");
    ungrouped.source_profile = "test".to_string();
    let mut env = create_test_env_empty();
    for instance in [worktree, ungrouped] {
        env.view.add_instance(instance);
    }
    env.view.group_by = crate::session::config::GroupByMode::Manual;
    env.view.flat_items = env.view.build_flat_items();
    env.view.update_selected();

    for (title, path) in [("worktree-session", "/tmp/repo"), ("ungrouped", "/tmp/u")] {
        let idx = env
            .view
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if env.view.get_instance(id).map(|i| i.title.as_str()) == Some(title)))
            .expect("session row should exist");
        env.view.cursor = idx;
        env.view.update_selected();
        env.view.new_dialog = None;

        env.view.handle_key(key(KeyCode::Char('N')), None);
        let dialog = env.view.new_dialog.as_ref().expect("N should open dialog");
        assert_eq!(dialog.path_value(), path, "{title}");
        assert_eq!(dialog.group_value(), "", "{title}");
    }
}

#[tokio::test]
#[serial]
async fn test_rename_selected_group_with_children() {
    use crate::session::GroupTree;

    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let storage = Storage::new_unwatched("test").unwrap();

    let mut inst1 = Instance::new("parent-session", "/tmp/p");
    inst1.group_path = "work".to_string();
    let mut inst2 = Instance::new("child-session", "/tmp/c");
    inst2.group_path = "work/frontend".to_string();
    let instances = vec![inst1, inst2];
    let mut group_tree = GroupTree::new_with_groups(&instances, &[]);
    group_tree.create_group("empty-group");
    storage
        .update(|i, g| {
            *i = instances.to_vec();
            *g = group_tree.get_all_groups();
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
    let profiles = crate::session::list_profiles().unwrap();
    let state = native_state(&profiles.iter().map(String::as_str).collect::<Vec<_>>()).await;
    apply_published(&mut view, &state).await;
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    for (old, new) in [("work", "projects"), ("empty-group", "renamed-group")] {
        view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
            old_path: old.to_string(),
            old_profile: "test".to_string(),
        });
        super::pickers_groups_sort::settle_rename_group(&state, &mut view, Some(new), None)
            .await
            .unwrap();
        let tree = view.group_trees.get("test").unwrap();
        assert!(
            !tree.group_exists(old),
            "old group path {old} should be gone"
        );
        assert!(tree.group_exists(new), "new group path {new} should exist");
    }

    let parent = view
        .instances()
        .find(|i| i.title == "parent-session")
        .unwrap();
    assert_eq!(parent.group_path, "projects");

    let child = view
        .instances()
        .find(|i| i.title == "child-session")
        .unwrap();
    assert_eq!(child.group_path, "projects/frontend");

    // Disk-state regression check: the rename must drop both old_path
    // and its descendant rows, leaving only the renamed paths on disk.
    let disk_groups: Vec<String> = storage
        .load_with_groups()
        .unwrap()
        .1
        .into_iter()
        .map(|g| g.path)
        .collect();
    assert!(
        !disk_groups.contains(&"work".to_string()),
        "old parent path must not survive on disk: {:?}",
        disk_groups
    );
    assert!(
        !disk_groups.contains(&"work/frontend".to_string()),
        "old descendant path must not survive on disk: {:?}",
        disk_groups
    );
    assert!(
        disk_groups.contains(&"projects".to_string()),
        "renamed parent must be on disk: {:?}",
        disk_groups
    );
    assert!(
        disk_groups.contains(&"projects/frontend".to_string()),
        "renamed descendant must be on disk: {:?}",
        disk_groups
    );
}

/// Renaming a group to its own path is a no-op, renaming onto an existing group fails, and a
/// real rename re-sorts the list.
#[tokio::test]
#[serial]
async fn test_rename_group_noop_and_duplicate() {
    let mut env = create_test_env_with_groups();
    let profiles = crate::session::list_profiles().unwrap();
    let state = native_state(&profiles.iter().map(String::as_str).collect::<Vec<_>>()).await;
    apply_published(&mut env.view, &state).await;
    let context = || crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    };

    env.view.group_rename_context = Some(context());
    super::pickers_groups_sort::settle_rename_group(&state, &mut env.view, Some("work"), None)
        .await
        .unwrap();
    let work_session = env
        .view
        .instances()
        .find(|i| i.title == "work-project")
        .unwrap();
    assert_eq!(work_session.group_path, "work");

    env.view.group_rename_context = Some(context());
    assert!(
        super::pickers_groups_sort::settle_rename_group(
            &state,
            &mut env.view,
            Some("personal"),
            None
        )
        .await
        .is_err(),
        "renaming to an existing group should fail"
    );

    env.view.sort_order = crate::session::config::SortOrder::AZ;
    env.view.group_rename_context = Some(context());
    super::pickers_groups_sort::settle_rename_group(&state, &mut env.view, Some("aaa"), None)
        .await
        .unwrap();
    let group_items: Vec<&str> = env
        .view
        .flat_items
        .iter()
        .filter_map(|item| match item {
            Item::Group { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        group_items,
        vec!["aaa", "personal"],
        "groups should be re-sorted alphabetically after rename"
    );
}

#[tokio::test]
#[serial]
async fn test_move_explicit_empty_group_between_profiles() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let source = Storage::new_unwatched("alpha").unwrap();
    source
        .update(|_instances, groups| {
            let mut group = Group::new("empty", "empty");
            group.collapsed = true;
            groups.push(group);
            Ok(())
        })
        .unwrap();
    let _target = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    let profiles = crate::session::list_profiles().unwrap();
    let state = native_state(&profiles.iter().map(String::as_str).collect::<Vec<_>>()).await;
    apply_published(&mut view, &state).await;
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "empty".to_string(),
        old_profile: "alpha".to_string(),
    });

    super::pickers_groups_sort::settle_rename_group(
        &state,
        &mut view,
        Some("moved-empty"),
        Some("beta"),
    )
    .await
    .unwrap();

    assert!(Storage::new_unwatched("alpha")
        .unwrap()
        .load_with_groups()
        .unwrap()
        .1
        .iter()
        .all(|group| group.path != "empty"));
    let moved = Storage::new_unwatched("beta")
        .unwrap()
        .load_with_groups()
        .unwrap()
        .1
        .into_iter()
        .find(|group| group.path == "moved-empty")
        .expect("empty group metadata moved to target profile");
    assert!(moved.collapsed);
}

#[tokio::test]
#[serial]
async fn test_group_profile_move_is_all_or_nothing() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let source = Storage::new_unwatched("alpha").unwrap();
    let mut first = Instance::new("first", "/tmp/first");
    first.group_path = "work".to_string();
    let mut second = Instance::new("second", "/tmp/second");
    second.group_path = "work".to_string();
    let mut work_group = Group::new("work", "work");
    work_group.collapsed = true;
    let source_empty = Group::new("keep-empty", "keep-empty");
    source
        .update(|instances, groups| {
            *instances = vec![first.clone(), second.clone()];
            *groups = vec![work_group.clone(), source_empty.clone()];
            Ok(())
        })
        .unwrap();
    let target = Storage::new_unwatched("beta").unwrap();
    let target_empty = Group::new("target-empty", "target-empty");
    target
        .update(|instances, groups| {
            instances.push(Instance::new("second", "/tmp/second/"));
            groups.push(target_empty.clone());
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view = HomeView::new_for_test(
        None,
        tools.clone(),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    let profiles = crate::session::list_profiles().unwrap();
    let state = native_state(&profiles.iter().map(String::as_str).collect::<Vec<_>>()).await;
    apply_published(&mut view, &state).await;
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });
    assert!(
        super::pickers_groups_sort::settle_rename_group(&state, &mut view, None, Some("beta"))
            .await
            .is_err()
    );
    assert_eq!(source.load().unwrap().len(), 2);
    assert_eq!(target.load().unwrap().len(), 1);
    let (_, source_groups) = source.load_with_groups().unwrap();
    let (_, target_groups) = target.load_with_groups().unwrap();
    assert_eq!(
        source_groups,
        vec![work_group.clone(), source_empty.clone()]
    );
    assert_eq!(target_groups, vec![target_empty.clone()]);

    target
        .update(|instances, _groups| {
            instances.clear();
            Ok(())
        })
        .unwrap();
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });
    super::pickers_groups_sort::settle_rename_group(&state, &mut view, None, Some("beta"))
        .await
        .unwrap();
    assert!(source.load().unwrap().is_empty());
    assert_eq!(target.load().unwrap().len(), 2);
    let published: Vec<_> = view
        .instances()
        .filter(|instance| instance.group_path == "work")
        .collect();
    assert_eq!(
        published.len(),
        2,
        "both members must be published in memory"
    );
    assert!(published
        .iter()
        .all(|instance| instance.source_profile == "beta"));
    let (_, source_groups) = source.load_with_groups().unwrap();
    assert_eq!(source_groups, vec![source_empty]);
    let (_, target_groups) = target.load_with_groups().unwrap();
    assert!(target_groups
        .iter()
        .any(|group| group.path == "work" && group.collapsed));
    assert!(target_groups
        .iter()
        .any(|group| group.path == "target-empty"));
    let reloaded =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    assert!(!reloaded.group_trees["alpha"].group_exists("work"));
    assert!(reloaded.group_trees["alpha"].group_exists("keep-empty"));
    assert!(reloaded.group_trees["beta"].group_exists("work"));
    assert!(reloaded.group_trees["beta"].group_exists("target-empty"));
}

#[tokio::test]
#[serial]
async fn group_profile_move_preflights_creating_and_expired_reservations() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let source = Storage::new_unwatched("alpha").unwrap();
    let mut first = Instance::new("first", "/tmp/preflight-first");
    first.group_path = "work".to_string();
    let mut second = Instance::new("second", "/tmp/preflight-second");
    second.group_path = "work".to_string();
    source
        .update(|instances, groups| {
            *instances = vec![first.clone(), second.clone()];
            groups.push(Group::new("work", "work"));
            Ok(())
        })
        .unwrap();
    let target = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    let state = native_state(&["alpha", "beta"]).await;
    apply_published(&mut view, &state).await;
    source
        .update(|rows, _| {
            rows.iter_mut()
                .find(|row| row.id == second.id)
                .unwrap()
                .status = Status::Creating;
            Ok(())
        })
        .unwrap();
    super::pickers_groups_sort::refresh_native_fixture(&state, &mut view).await;
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });

    let error = super::pickers_groups_sort::settle_rename_group(
        &state,
        &mut view,
        Some("moved"),
        Some("beta"),
    )
    .await
    .expect_err("a creating member must reject the complete group move");

    assert!(
        view.info_dialog.is_some(),
        "creating-member rejection is surfaced"
    );
    drop(error);
    assert!(view
        .instances()
        .filter(|instance| instance.id == first.id || instance.id == second.id)
        .all(|instance| instance.source_profile == "alpha" && instance.group_path == "work"));
    let source_rows = source.load().unwrap();
    assert_eq!(source_rows.len(), 2);
    assert!(source_rows
        .iter()
        .all(|instance| instance.group_path == "work"));
    assert!(target.load().unwrap().is_empty());

    source
        .update(|rows, _| {
            rows.iter_mut()
                .find(|row| row.id == second.id)
                .unwrap()
                .status = Status::Deleting;
            Ok(())
        })
        .unwrap();
    super::pickers_groups_sort::refresh_native_fixture(&state, &mut view).await;
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });
    let error = super::pickers_groups_sort::settle_rename_group(
        &state,
        &mut view,
        Some("moved"),
        Some("beta"),
    )
    .await
    .expect_err("a deleting member must reject the complete group move");
    assert!(
        view.info_dialog.is_some(),
        "deleting-member rejection is surfaced"
    );
    drop(error);
    assert_eq!(source.load().unwrap().len(), 2);
    assert!(target.load().unwrap().is_empty());

    let stale = LifecycleReservation {
        op: LifecycleOperation::Launch,
        generation: 1,
        at: chrono::Utc::now() - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1),
    };
    source
        .update(|instances, _groups| {
            let instance = instances
                .iter_mut()
                .find(|instance| instance.id == second.id)
                .unwrap();
            instance.status = Status::Idle;
            instance.lifecycle_generation = 1;
            instance.lifecycle_reservation = Some(stale);
            Ok(())
        })
        .unwrap();
    super::pickers_groups_sort::refresh_native_fixture(&state, &mut view).await;
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });

    super::pickers_groups_sort::settle_rename_group(&state, &mut view, Some("moved"), Some("beta"))
        .await
        .expect("expired reservation must not block the group move");
    assert!(source.load().unwrap().is_empty());
    assert_eq!(target.load().unwrap().len(), 2);
}

#[test]
#[serial]
fn test_q_in_search_mode_types_q_not_quit() {
    let env = create_test_env_with_sessions(3);
    let mut view = env.view;

    assert!(!view.has_dialog());
    view.handle_key(key(KeyCode::Char('/')), None);
    assert!(view.search_active);
    assert!(view.has_dialog(), "active search counts as a dialog");

    let action = view.handle_key(key(KeyCode::Char('q')), None);
    assert_eq!(action, None);
    assert!(view.search_active);
    assert_eq!(view.search_query.value(), "q");
}

#[test]
fn test_project_group_key_scratch_uses_sentinel_not_label() {
    use crate::session::{project_group_display_name, SCRATCH_GROUP_PATH};
    use crate::tui::home::project_group_key;

    let mut inst = Instance::new(
        "test",
        "/home/user/.config/agent-of-empires/scratch/a4535853054b4096",
    );
    inst.scratch = true;
    // Scratch keys on the sentinel identity, not the display label, so a real
    // repo named `scratch` keeps a distinct identity (#3237).
    assert_eq!(project_group_key(&inst), SCRATCH_GROUP_PATH);
    assert_eq!(project_group_display_name(SCRATCH_GROUP_PATH), "Scratch");
}

#[tokio::test]
#[serial]
async fn test_cursor_follows_session_after_deletion() {
    let mut env = create_test_env_with_sessions(4);
    let state = native_state(&["test"]).await;
    apply_published(&mut env.view, &state).await;

    // Cursor starts at 0; move it to index 2 (session2)
    env.view.cursor = 2;
    env.view.update_selected();
    let tracked_id = env.view.selected_session.clone().unwrap();

    // Delete item at index 1 (a session above the cursor)
    let victim_id = match &env.view.flat_items[1] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("expected session at index 1"),
    };
    let (status, _, body) = request_with_headers(
        &state,
        "DELETE",
        &format!("/api/sessions/{victim_id}"),
        serde_json::json!({}),
    )
    .await;
    assert!(status.is_success(), "{status}: {body}");
    assert!(env.view.get_instance(&victim_id).is_some());
    apply_published(&mut env.view, &state).await;
    assert!(Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .iter()
        .all(|row| row.id != victim_id));

    // Cursor should have followed the tracked session to its new position
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(tracked_id.as_str())
    );
    assert_eq!(env.view.cursor, 1);
}
