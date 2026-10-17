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

/// The fork seed forks the parent's agent, so the dialog opens preselected on it rather
/// than the configured default: a Codex parent must land on codex, or the dialog's tool and
/// the seed disagree.
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

/// A structured parent forks via the ACP `session/fork` handshake, so the seed must be
/// `Structured` carrying the parent's captured ACP session id, not a terminal
/// resume-with-fork-flag seed.
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
/// the duration picker, on a snoozed one it wakes immediately.
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

    env.view.snooze_session_for(&id, 60).unwrap();
    env.view
        .dispatch_context_menu_action(ContextMenuAction::ToggleSnooze);
    assert!(
        env.view.snooze_duration_dialog.is_none(),
        "waking a snoozed session must not open the duration picker"
    );
    assert!(
        !env.view.instances.get(&id).is_some_and(|i| i.is_snoozed()),
        "context-menu Snooze on a snoozed session must wake it immediately"
    );

    env.view.sort_order = SortOrder::Attention;
    env.view.flat_items = env.view.build_flat_items();
    env.view.select_session_by_id(&id);
    env.view
        .dispatch_context_menu_action(ContextMenuAction::ToggleSnooze);
    assert!(
        env.view.snooze_duration_dialog.is_some(),
        "context-menu Snooze on an active session must open the duration picker"
    );
}

/// `N` prefills the new-session dialog from the selected session: a worktree row borrows its
/// main repo path, an ungrouped row its own path with no group.
#[test]
#[serial]
fn test_shift_n_prefills_from_selected_session() {
    let mut worktree = Instance::new("worktree-session", "/tmp/repo-worktrees/feature-branch");
    worktree.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "feature-branch".to_string(),
        main_repo_path: "/tmp/repo".to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    });
    let mut env = seeded_env(
        test_home(),
        &[worktree, Instance::new("ungrouped", "/tmp/u")],
        true,
    );

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

#[test]
#[serial]
fn test_rename_selected_group_with_children() {
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
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.flat_items = view.build_flat_items();
    view.update_selected();

    for (old, new) in [("work", "projects"), ("empty-group", "renamed-group")] {
        view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
            old_path: old.to_string(),
            old_profile: "test".to_string(),
        });
        view.rename_selected_group(Some(new), None).unwrap();
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
#[test]
#[serial]
fn test_rename_group_noop_and_duplicate() {
    let mut env = create_test_env_with_groups();
    let context = || crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "test".to_string(),
    };

    env.view.group_rename_context = Some(context());
    env.view.rename_selected_group(Some("work"), None).unwrap();
    let work_session = env
        .view
        .instances()
        .find(|i| i.title == "work-project")
        .unwrap();
    assert_eq!(work_session.group_path, "work");

    env.view.group_rename_context = Some(context());
    assert!(
        env.view
            .rename_selected_group(Some("personal"), None)
            .is_err(),
        "renaming to an existing group should fail"
    );

    env.view.sort_order = crate::session::config::SortOrder::AZ;
    env.view.group_rename_context = Some(context());
    env.view.rename_selected_group(Some("aaa"), None).unwrap();
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

#[test]
#[serial]
fn test_move_explicit_empty_group_between_profiles() {
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
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "empty".to_string(),
        old_profile: "alpha".to_string(),
    });

    view.rename_selected_group(Some("moved-empty"), Some("beta"))
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

#[test]
#[serial]
fn test_group_profile_move_rejects_concurrent_fresh_member_without_metadata_split() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let source = Storage::new_unwatched("alpha").unwrap();
    let mut known = Instance::new("known", "/tmp/known");
    known.group_path = "team".to_string();
    source
        .update(|instances, groups| {
            instances.push(known.clone());
            groups.push(Group::new("team", "team"));
            Ok(())
        })
        .unwrap();
    let _target = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.group_by = crate::session::config::GroupByMode::Manual;
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "team".to_string(),
        old_profile: "alpha".to_string(),
    });

    source
        .update(|instances, _groups| {
            let mut concurrent = Instance::new("concurrent", "/tmp/concurrent");
            concurrent.group_path = "team/fresh".to_string();
            instances.push(concurrent);
            Ok(())
        })
        .unwrap();
    let error = view
        .rename_selected_group(Some("moved-team"), Some("beta"))
        .expect_err("a concurrent group member must abort the move");
    let message = format!("{error:#}");
    assert!(
        message.contains("group membership changed while the cross-profile move was pending"),
        "unexpected profile-move rejection: {message}"
    );
    let (source_rows, source_groups) = source.load_with_groups().unwrap();
    assert_eq!(source_rows.len(), 2);
    assert!(source_rows
        .iter()
        .all(|instance| instance.group_path.starts_with("team")));
    assert!(source_groups.iter().any(|group| group.path == "team"));
    assert!(Storage::new_unwatched("beta")
        .unwrap()
        .load()
        .unwrap()
        .is_empty());
}

#[test]
#[serial]
fn test_group_profile_move_is_all_or_nothing() {
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
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });
    assert!(view.rename_selected_group(None, Some("beta")).is_err());
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
    view.rename_selected_group(None, Some("beta")).unwrap();
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

#[test]
#[serial]
fn group_profile_move_preflights_creating_and_expired_reservations() {
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
    view.mutate_instance(&second.id, |instance| {
        instance.status = Status::Creating;
    });
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });

    let error = view
        .rename_selected_group(Some("moved"), Some("beta"))
        .expect_err("a creating member must reject the complete group move");

    assert!(error.to_string().contains("being created"));
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

    view.mutate_instance(&second.id, |instance| {
        instance.status = Status::Deleting;
    });
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });
    let error = view
        .rename_selected_group(Some("moved"), Some("beta"))
        .expect_err("a deleting member must reject the complete group move");
    assert!(error.to_string().contains("being deleted"));
    assert_eq!(source.load().unwrap().len(), 2);
    assert!(target.load().unwrap().is_empty());

    let stale = LifecycleReservation {
        op: LifecycleOperation::Launch,
        generation: 1,
        at: chrono::Utc::now() - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1),
        resource_intent: None,
    };
    view.mutate_instance(&second.id, |instance| {
        instance.status = Status::Idle;
        instance.lifecycle_generation = 1;
        instance.lifecycle_reservation = Some(stale.clone());
    });
    source
        .update(|instances, _groups| {
            let instance = instances
                .iter_mut()
                .find(|instance| instance.id == second.id)
                .unwrap();
            instance.lifecycle_generation = 1;
            instance.lifecycle_reservation = Some(stale);
            Ok(())
        })
        .unwrap();
    view.group_rename_context = Some(crate::tui::home::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });

    view.rename_selected_group(Some("moved"), Some("beta"))
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
#[serial]
fn queued_creation_result_does_not_block_exclusive_ownership() {
    let mut env = setup_creation_test_env();
    let queued = env.view.creation_poller.observe_next_queued_result();
    env.view
        .request_creation(creation_data(&env.project_dir, "Queued", ""), None);
    queued
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("worker queued its real result");
    let (acquired_tx, acquired_rx) = std::sync::mpsc::channel();
    let peer = std::thread::spawn(move || {
        let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
        acquired_tx.send(()).unwrap();
        drop(ownership);
    });
    let acquired_while_queued = acquired_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .is_ok();
    // Consume before asserting so even the broken implementation releases its
    // queued guard and lets the peer retire rather than stranding a test thread.
    let created = drain_creation_result(&mut env.view);
    peer.join().unwrap();
    assert!(
        acquired_while_queued,
        "the UI must be able to take exclusive ownership before polling"
    );
    assert!(created.is_some());
}

#[test]
#[serial]
fn queued_creation_cannot_publish_into_a_replacement_profile() {
    let mut env = setup_creation_test_env();
    let queued = env.view.creation_poller.observe_next_queued_result();
    env.view.request_creation(
        creation_data(&env.project_dir, "Original profile", ""),
        None,
    );
    queued
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("worker queued its real result");
    let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
    let profile_dir = env.storage.sessions_path().parent().unwrap().to_path_buf();
    std::fs::rename(&profile_dir, env._temp.path().join("retired-profile")).unwrap();
    std::fs::create_dir_all(&profile_dir).unwrap();
    let replacement = Storage::open_unwatched_with_ownership("default", &ownership).unwrap();
    let peer = Instance::new(
        "replacement profile owner",
        &env.project_dir.to_string_lossy(),
    );
    let peer_id = peer.id.clone();
    replacement
        .update_with_ownership(&ownership, |rows, _| {
            rows.push(peer);
            Ok(())
        })
        .unwrap();
    drop(ownership);
    assert_eq!(drain_creation_result(&mut env.view), None);
    assert!(env.view.info_dialog.is_some());
    let rows = replacement.load().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].id, peer_id);
}

/// A committed async creation stays deleted after a peer removes its row.
#[test]
#[serial]
fn apply_creation_results_finalizes_persisted_stub() {
    let CreationTestEnv {
        mut view,
        storage,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let initial_sessions_created = crate::session::config::AppStateConfig::load()
        .unwrap()
        .sessions_created;
    let queued = view.creation_poller.observe_next_queued_result();
    view.request_creation(
        creation_data(&project_dir, "Async Test", "async-success"),
        None,
    );
    assert!(view.is_creation_pending());
    let stub_id = view
        .creating_stub_id
        .clone()
        .expect("canonical Creating row");
    view.save().unwrap();
    let (creating, creating_groups) = storage.load_with_groups().unwrap();
    assert_eq!(creating.len(), 1);
    let stub = &creating[0];
    assert_eq!(stub.id, stub_id);
    assert_eq!(stub.status, Status::Creating);
    assert!(stub.lifecycle_reservation_is_owned(
        crate::session::LifecycleOperation::Create,
        stub.lifecycle_generation
    ));
    assert!(creating_groups
        .iter()
        .any(|group| group.path == "async-success"));
    queued
        .recv_timeout(std::time::Duration::from_secs(10))
        .expect("real producer result queued");
    let session_id = drain_creation_result(&mut view).expect("creation finalized");
    assert_eq!(session_id, stub_id);
    assert_eq!(
        crate::session::config::AppStateConfig::load()
            .unwrap()
            .sessions_created,
        initial_sessions_created + 1
    );
    assert!(view.creating_provisional_group_paths.is_empty());
    assert!(!view
        .pending_added
        .get("default")
        .is_some_and(|pending| pending.contains(&session_id)));
    let (finalized, groups) = storage.load_with_groups().unwrap();
    assert_eq!(finalized.len(), 1);
    let row = &finalized[0];
    assert_eq!(row.id, stub.id);
    assert_eq!(row.created_at, stub.created_at);
    assert_eq!(row.lifecycle_generation, stub.lifecycle_generation);
    assert_ne!(row.status, Status::Creating);
    assert_eq!(row.lifecycle_reservation, None);
    let live = view
        .get_instance(&stub_id)
        .expect("same identity finalized in memory");
    assert_eq!(live.created_at, stub.created_at);
    assert_eq!(live.lifecycle_generation, stub.lifecycle_generation);
    assert_ne!(live.status, Status::Creating);
    assert_eq!(live.lifecycle_reservation, None);
    assert!(groups.iter().any(|group| group.path == "async-success"));
    storage
        .update(|rows, _| {
            rows.retain(|row| row.id != session_id);
            Ok(())
        })
        .unwrap();
    view.save().unwrap();
    assert!(
        view.get_instance(&session_id).is_none(),
        "save evicts the peer-deleted finalized row"
    );
    assert!(
        storage
            .load()
            .unwrap()
            .iter()
            .all(|row| row.id != session_id),
        "save must not resurrect a committed row"
    );
}

/// A cancelled request must stay cancelled when another is queued behind it, and its
/// result must not consume the newer request's stub.
#[test]
#[serial]
fn cancelled_creation_is_not_revived_by_a_later_request() {
    let CreationTestEnv {
        mut view,
        storage,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();

    let mut cancelled = creation_data(&project_dir, "Cancelled", "");
    cancelled.worktree_enabled = true;
    cancelled.create_new_branch = true;
    cancelled.worktree_branch = Some("cancelled-branch".to_string());
    view.request_creation(cancelled, None);
    view.cancel_creation();
    view.request_creation(creation_data(&project_dir, "Kept", ""), None);

    let session_id = drain_creation_result(&mut view).expect("the later request should finish");
    assert_eq!(view.get_instance(&session_id).unwrap().title, "Kept");
    assert!(!view.is_creation_pending());
    let persisted = storage.load().unwrap();
    assert_eq!(
        persisted
            .iter()
            .map(|row| row.title.as_str())
            .collect::<Vec<_>>(),
        ["Kept"]
    );
    let repo = git2::Repository::open(&project_dir).unwrap();
    assert!(
        repo.find_branch("cancelled-branch", git2::BranchType::Local)
            .is_err(),
        "the cancelled request's worktree branch must be rolled back"
    );
}

/// Ctrl-C while on_create runs lets that hook finish but must not start on_launch.
#[test]
#[serial]
fn cancel_during_on_create_skips_on_launch() {
    let CreationTestEnv {
        mut view,
        storage,
        project_dir,
        _guard,
        _temp,
    } = setup_creation_test_env();
    let Ok(sh) = which::which("sh") else {
        eprintln!("skipping: sh not found on PATH");
        return;
    };
    let _shell = crate::session::test_support::EnvGuard::set(&[("SHELL", sh)]);
    // on_create signals that it started, then holds until the test releases it.
    let hooks = crate::session::config::repo_config::ResolvedHooks::with_repo(
        "default",
        &project_dir,
        crate::session::config::repo_config::HooksConfig {
            on_create: vec!["touch create-started; i=0; while [ ! -e release ] && [ $i -lt 1000 ]; do sleep 0.01; i=$((i+1)); done; [ -e release ]".to_string()],
            on_launch: vec!["touch launch-started".to_string()],
            ..Default::default()
        },
    );
    view.request_creation(creation_data(&project_dir, "Hooked", ""), hooks);

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !project_dir.join("create-started").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "on_create never started"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let pending = storage
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.title == "Hooked")
        .unwrap();
    assert!(pending
        .lifecycle_reservation
        .as_ref()
        .unwrap()
        .resource_intent
        .is_some());
    view.cancel_creation();
    assert!(view.get_instance(&pending.id).is_none());
    view.reload().unwrap();
    view.save().unwrap();
    assert!(view.get_instance(&pending.id).is_none());
    let durable = storage
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.id == pending.id)
        .unwrap();
    assert_eq!(durable.created_at, pending.created_at);
    assert_eq!(durable.lifecycle_reservation, pending.lifecycle_reservation);
    std::fs::write(project_dir.join("release"), b"").unwrap();

    assert_eq!(drain_creation_result(&mut view), None);
    assert_eq!(
        crate::session::config::AppStateConfig::load()
            .unwrap()
            .sessions_created,
        0
    );
    assert!(!view.is_creation_pending());
    assert!(
        !project_dir.join("launch-started").exists(),
        "on_launch must not start after a cancel during on_create"
    );
    assert!(storage.load().unwrap().is_empty());
}

#[test]
#[serial]
fn saving_creating_stub_during_native_checkout_preserves_claim_authority() {
    use std::os::unix::fs::PermissionsExt;
    struct ReleaseNative<'a>(&'a std::path::Path);
    impl Drop for ReleaseNative<'_> {
        fn drop(&mut self) {
            let _ = std::fs::write(self.0, b"release");
        }
    }
    for claimant in [
        "none",
        "foreign",
        "redirected",
        "queued",
        "shutdown-unchanged",
        "shutdown-redirected",
        "shutdown-runtime-status",
    ] {
        let mut env = setup_creation_test_env();
        let entered = env._temp.path().join("native-entered");
        let release = env._temp.path().join("native-release");
        let hook = env.project_dir.join(".git/hooks/post-checkout");
        let quote = crate::session::environment::shell_escape;
        std::fs::write(
            &hook,
            format!(
                r#"#!/bin/sh
pwd -P > {}.tmp || exit 1
mv {}.tmp {} || exit 1
while [ ! -f {} ] && [ -d {} ]; do sleep 0.01; done
"#,
                quote(&entered.to_string_lossy()),
                quote(&entered.to_string_lossy()),
                quote(&entered.to_string_lossy()),
                quote(&release.to_string_lossy()),
                quote(&env._temp.path().to_string_lossy()),
            ),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _release_native = ReleaseNative(&release);
        let queued = env.view.creation_poller.observe_next_queued_result();
        let mut data = creation_data(&env.project_dir, "Native stub", "");
        data.worktree_enabled = true;
        data.worktree_branch = Some("native-stub".into());
        data.create_new_branch = true;
        env.view.request_creation(data, None);
        let stub_id = env.view.creating_stub_id.clone().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !entered.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "native callback did not enter"
            );
            std::thread::yield_now();
        }
        let target = std::path::PathBuf::from(
            std::fs::read_to_string(&entered)
                .unwrap()
                .trim_end_matches('\n'),
        );
        env.view.save().unwrap();
        let saved = env.storage.load().unwrap();
        assert_eq!(saved.len(), 1);
        let canonical = &saved[0];
        assert_eq!(canonical.id, stub_id);
        assert_eq!(canonical.status, Status::Creating);
        assert_eq!(
            std::path::Path::new(&canonical.project_path)
                .canonicalize()
                .unwrap(),
            target.canonicalize().unwrap(),
        );
        assert!(canonical
            .lifecycle_reservation
            .as_ref()
            .unwrap()
            .resource_intent
            .is_some());
        let redirected = env._temp.path().join("unrelated-checkout");
        std::fs::create_dir(&redirected).unwrap();
        std::fs::write(redirected.join("keep"), b"unrelated").unwrap();
        let before = std::fs::read(env.storage.sessions_path()).unwrap();
        match claimant {
            "foreign" => {
                let mut owner = Instance::new("Foreign creator", target.to_str().unwrap());
                owner.source_profile = "default".into();
                owner.worktree_info = canonical.worktree_info.clone();
                let error = env
                    .storage
                    .update(|rows, _| {
                        rows.push(owner);
                        Ok(())
                    })
                    .expect_err("reserved checkout cannot be adopted");
                assert!(error.to_string().contains("busy"), "{error:#}");
                assert_eq!(std::fs::read(env.storage.sessions_path()).unwrap(), before);
            }
            "redirected" | "shutdown-redirected" => {
                let error = env
                    .storage
                    .update(|rows, _| {
                        rows.iter_mut()
                            .find(|row| row.id == stub_id)
                            .unwrap()
                            .project_path = redirected.to_string_lossy().into_owned();
                        Ok(())
                    })
                    .expect_err("reserved owner cannot be redirected");
                assert!(error.to_string().contains("busy"), "{error:#}");
                assert_eq!(std::fs::read(env.storage.sessions_path()).unwrap(), before);
            }
            "shutdown-runtime-status" => {
                env.storage
                    .update(|rows, _| {
                        rows.iter_mut()
                            .find(|row| row.id == stub_id)
                            .unwrap()
                            .status = Status::Running;
                        Ok(())
                    })
                    .unwrap();
                let rows = env.storage.load().unwrap();
                assert_eq!(rows[0].id, canonical.id);
                assert_eq!(rows[0].created_at, canonical.created_at);
                assert_eq!(rows[0].lifecycle_generation, canonical.lifecycle_generation);
                assert_eq!(
                    rows[0].lifecycle_reservation,
                    canonical.lifecycle_reservation
                );
            }
            _ => {}
        }
        std::fs::write(&release, b"release").unwrap();
        queued
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("real producer result queued");
        if claimant == "queued" {
            let before = std::fs::read(env.storage.sessions_path()).unwrap();
            let error = env
                .storage
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == stub_id)
                        .unwrap()
                        .project_path = redirected.to_string_lossy().into_owned();
                    Ok(())
                })
                .expect_err("queued result still owns its resource intent");
            assert!(error.to_string().contains("busy"), "{error:#}");
            assert_eq!(std::fs::read(env.storage.sessions_path()).unwrap(), before);
        }
        if claimant.starts_with("shutdown-") {
            env.view.cleanup_pending_creation();
            env.view.save().unwrap();
            assert!(env
                .storage
                .load()
                .unwrap()
                .iter()
                .all(|row| row.id != stub_id));
            assert!(env.view.get_instance(&stub_id).is_none());
            assert!(!target.exists());
            assert!(git2::Repository::open(&env.project_dir)
                .unwrap()
                .find_branch("native-stub", git2::BranchType::Local)
                .is_err());
        } else {
            assert_eq!(drain_creation_result(&mut env.view), Some(stub_id.clone()));
            env.view.save().unwrap();
            let rows = env.storage.load().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, canonical.id);
            assert_eq!(rows[0].created_at, canonical.created_at);
            assert_eq!(rows[0].lifecycle_generation, canonical.lifecycle_generation);
            assert_ne!(rows[0].status, Status::Creating);
            assert_eq!(rows[0].lifecycle_reservation, None);
            assert_eq!(std::fs::read(target.join("README.md")).unwrap(), b"test\n");
            assert!(git2::Repository::open(&env.project_dir)
                .unwrap()
                .find_branch("native-stub", git2::BranchType::Local)
                .is_ok());
        }
        assert_eq!(
            std::fs::read(redirected.join("keep")).unwrap(),
            b"unrelated"
        );
    }
}

/// A publication collision retains the committed owner and pre-existing groups.
#[test]
#[serial]
fn apply_creation_results_rolls_back_on_peer_collision() {
    for managed in [false, true] {
        let mut env = setup_creation_test_env();
        let parent = "existing-empty";
        let child = "existing-empty/collision";
        env.view
            .group_trees
            .get_mut("default")
            .unwrap()
            .create_group(parent);
        env.view.save().unwrap();
        let mut data = creation_data(&env.project_dir, "Raced title", child);
        data.worktree_enabled = managed;
        if managed {
            data.worktree_branch = Some("raced-worktree".into());
            data.create_new_branch = true;
        }
        let queued = env.view.creation_poller.observe_next_queued_result();
        env.view.request_creation(data, None);
        let id = env.view.creating_stub_id.clone().unwrap();
        env.view.save().unwrap();
        queued
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("real producer result queued");
        let (rows, groups) = env.storage.load_with_groups().unwrap();
        assert_eq!(rows.len(), 1);
        let canonical = &rows[0];
        assert_eq!(canonical.id, id);
        assert_eq!(canonical.status, Status::Creating);
        assert!(groups.iter().any(|group| group.path == child));
        let path = std::path::PathBuf::from(&canonical.project_path);
        std::fs::write(path.join("winner-marker"), b"keep").unwrap();
        let mut owner = Instance::new("Raced title", path.to_str().unwrap());
        owner.source_profile = "default".into();
        owner.worktree_info = canonical.worktree_info.clone();
        if let Some(info) = &mut owner.worktree_info {
            info.managed_by_aoe = false;
        }
        let owner_id = owner.id.clone();
        let before = std::fs::read(env.storage.sessions_path()).unwrap();
        let insertion = env.storage.update(|rows, _| {
            rows.push(owner);
            Ok(())
        });
        if managed {
            let error = insertion.expect_err("managed checkout is reserved until publication");
            assert!(error.to_string().contains("busy"), "{error:#}");
            assert_eq!(std::fs::read(env.storage.sessions_path()).unwrap(), before);
            assert!(canonical
                .lifecycle_reservation
                .as_ref()
                .unwrap()
                .resource_intent
                .is_some());
            assert_eq!(drain_creation_result(&mut env.view), Some(id.clone()));
        } else {
            insertion.expect("unreserved plain directory permits peer adoption");
            assert!(canonical
                .lifecycle_reservation
                .as_ref()
                .unwrap()
                .resource_intent
                .is_none());
            assert_eq!(drain_creation_result(&mut env.view), None);
            assert!(env.view.info_dialog.is_some());
            assert_eq!(
                env.view
                    .get_instance(&owner_id)
                    .map(|row| row.project_path.as_str()),
                path.to_str()
            );
            assert!(env.view.get_instance(&id).is_none());
        }
        assert!(env.view.creating_provisional_group_paths.is_empty());
        assert_eq!(std::fs::read(path.join("winner-marker")).unwrap(), b"keep");
        assert_eq!(std::fs::read(path.join("README.md")).unwrap(), b"test\n");
        env.view.save().unwrap();
        let (persisted, groups) = env.storage.load_with_groups().unwrap();
        assert_eq!(persisted.len(), 1);
        let expected_id = if managed { &id } else { &owner_id };
        assert_eq!(&persisted[0].id, expected_id);
        assert_ne!(persisted[0].status, Status::Creating);
        assert_eq!(persisted[0].lifecycle_reservation, None);
        assert!(groups.iter().any(|group| group.path == parent));
        assert_eq!(groups.iter().any(|group| group.path == child), managed);
        assert_eq!(
            env.view
                .group_trees
                .get("default")
                .unwrap()
                .group_exists(child),
            managed
        );
        if managed {
            assert_eq!(persisted[0].created_at, canonical.created_at);
            assert_eq!(
                persisted[0].lifecycle_generation,
                canonical.lifecycle_generation
            );
            let git = crate::git::GitWorktree::new(env.project_dir.clone()).unwrap();
            assert!(git
                .list_worktrees()
                .unwrap()
                .iter()
                .any(|entry| entry.path == path));
            assert!(git.branch_exists("raced-worktree").unwrap());
        }
    }
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

#[test]
#[serial]
fn test_cursor_follows_session_after_deletion() {
    let mut env = create_test_env_with_sessions(4);

    // Cursor starts at 0; move it to index 2 (session2)
    env.view.cursor = 2;
    env.view.update_selected();
    let tracked_id = env.view.selected_session.clone().unwrap();

    // Delete item at index 1 (a session above the cursor)
    let victim_id = match &env.view.flat_items[1] {
        Item::Session { id, .. } => id.clone(),
        _ => panic!("expected session at index 1"),
    };
    env.view.remove_instance(&victim_id);
    env.view.rebuild_group_trees();
    let _ = env.view.save();
    env.view.reload().unwrap();

    // Cursor should have followed the tracked session to its new position
    assert_eq!(
        env.view.selected_session.as_deref(),
        Some(tracked_id.as_str())
    );
    assert_eq!(env.view.cursor, 1);
}

#[test]
#[serial]
fn sandbox_pull_releases_root_and_revalidates_creation() {
    use crate::session::storage::acquire_ownership_lock;
    use crate::tui::creation_poller::{CreationPoller, CreationRequest, CreationResult};
    use std::time::{Duration, Instant};
    for scenario in ["success", "retry", "profile", "checkout", "cancel"] {
        let env = setup_creation_test_env();
        let root = env._temp.path();
        let ready = root.join("pull-ready");
        let release = root.join("pull-release");
        struct ReleasePull<'a>(&'a std::path::Path);
        impl Drop for ReleasePull<'_> {
            fn drop(&mut self) {
                let _ = std::fs::write(self.0, b"release");
            }
        }
        let _release_pull = ReleasePull(&release);
        let escape = crate::session::environment::shell_escape;
        let script = format!(
            r#"#!/bin/sh
case "$1" in
  context) printf '%s' '{{"Name":"default","Endpoints":{{"docker":{{"Host":"unix:///var/run/docker.sock"}}}}}}' ;;
  info|--version) exit 0 ;;
  container)
    case "$2" in
      inspect) printf '%s\n' 'Error response from daemon: No such container: fixture' >&2; exit 1 ;;
      rm|stop|start) exit 0 ;;
      *) printf 'unsupported container command: %s\n' "$*" >&2; exit 64 ;;
    esac ;;
  inspect) printf '%s\n' 'Error response from daemon: No such container: fixture' >&2; exit 1 ;;
  image) exit 1 ;;
  pull)
    if [ '{scenario}' = retry ] && [ ! -f {attempt} ]; then : > {attempt}; exit 1; fi
    : > {ready}
    while [ ! -f {release} ]; do sleep 0.01; done ;;
  run|create) printf '%s\n' fixture-container ;;
  start|stop|rm) exit 0 ;;
  exec) exit 1 ;;
  *) printf 'unsupported fixture command: %s\n' "$*" >&2; exit 64 ;;
esac
"#,
            attempt = escape(&root.join("pull-attempt").to_string_lossy()),
            ready = escape(&ready.to_string_lossy()),
            release = escape(&release.to_string_lossy()),
        );
        let _docker =
            crate::session::test_support::install_login_shell_path_command(root, "docker", &script);
        let _routing = crate::session::test_support::EnvGuard::unset(&[
            "DOCKER_HOST",
            "DOCKER_CONTEXT",
            "DOCKER_TLS",
            "DOCKER_TLS_VERIFY",
            "DOCKER_CERT_PATH",
        ]);
        let mut config = crate::session::Config::default();
        config.sandbox.container_runtime = crate::session::ContainerRuntimeName::Docker;
        std::fs::write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            toml::to_string(&config).unwrap(),
        )
        .unwrap();
        let mut data = creation_data(&env.project_dir, "sandbox", "");
        data.sandbox = true;
        data.sandbox_image = "fixture-image".into();
        data.command_override = "true".into();
        let hooks = if scenario == "retry" {
            let mut hooks = crate::session::config::repo_config::HooksConfig::default();
            hooks.on_launch.push("true".into());
            crate::session::config::repo_config::ResolvedHooks::with_repo(
                "default",
                &env.project_dir,
                hooks,
            )
        } else {
            None
        };
        let mut stub = Instance::new(&data.title, &data.path);
        stub.source_profile = data.profile.clone();
        stub.status = Status::Creating;
        stub.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )
        .unwrap();
        let provisional = std::sync::Arc::new(
            crate::session::builder::ProvisionalCreation::capture(&env.storage, &stub).unwrap(),
        );
        env.storage
            .update(|rows, _| {
                rows.push(stub.clone());
                Ok(())
            })
            .unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut poller = CreationPoller::new();
        poller
            .request_creation(CreationRequest {
                data,
                existing_instances: vec![],
                hooks,
                cancel: cancel.clone(),
                provisional_creation: Some(provisional),
            })
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::yield_now();
        }
        let entered_pull = ready.exists();
        let (sender, receiver) = std::sync::mpsc::channel();
        let peer = std::thread::spawn(move || {
            let _ = sender.send(acquire_ownership_lock().unwrap());
        });
        let ownership = receiver.recv_timeout(Duration::from_secs(2)).ok();
        let available_during_pull = ownership.is_some();
        let mut replacement_bytes = None;
        if let Some(ownership) = ownership {
            match scenario {
                "profile" => {
                    let profile_dir = env.storage.sessions_path().parent().unwrap().to_path_buf();
                    std::fs::rename(&profile_dir, root.join("retired-profile")).unwrap();
                    std::fs::create_dir_all(&profile_dir).unwrap();
                    let replacement =
                        Storage::open_unwatched_with_ownership("default", &ownership).unwrap();
                    replacement
                        .update_with_ownership(&ownership, |rows, _| {
                            rows.push(Instance::new("peer", &env.project_dir.to_string_lossy()));
                            Ok(())
                        })
                        .unwrap();
                    replacement_bytes = Some(std::fs::read(replacement.sessions_path()).unwrap());
                }
                "checkout" => {
                    std::fs::rename(&env.project_dir, root.join("moved-project")).unwrap()
                }
                "cancel" => cancel.cancel(),
                _ => {}
            }
            drop(ownership);
        }
        std::fs::write(&release, b"release").unwrap();
        let outcome = poller
            .recv_result_timeout(Duration::from_secs(10))
            .expect("producer completed after pull release");
        drop(receiver);
        peer.join().unwrap();
        assert!(
            entered_pull,
            "{scenario}: producer must reach supervised pull: {:?}",
            outcome.result
        );
        assert!(
            available_during_pull,
            "{scenario}: unrelated ownership mutations must progress during Docker wait"
        );
        match (scenario, outcome.result) {
            ("success" | "retry", CreationResult::Success(mut success)) => {
                success.build_result.publish().unwrap();
                let rows = env.storage.load().unwrap();
                assert_eq!(rows.len(), 1);
                assert_eq!(rows[0].id, stub.id);
                assert_eq!(rows[0].created_at, stub.created_at);
                assert_eq!(rows[0].lifecycle_generation, stub.lifecycle_generation);
                assert_ne!(rows[0].status, Status::Creating);
                assert_eq!(rows[0].lifecycle_reservation, None);
            }
            ("checkout", CreationResult::Success(mut success)) => {
                assert!(success.build_result.publish().is_err());
                success.build_result.rollback().unwrap();
                assert!(env.storage.load().unwrap().is_empty());
                assert_eq!(
                    std::fs::read(root.join("moved-project/README.md")).unwrap(),
                    b"test\n"
                );
            }
            ("profile", CreationResult::Error(_)) => {
                let bytes = replacement_bytes.expect("replacement profile persisted");
                assert_eq!(std::fs::read(env.storage.sessions_path()).unwrap(), bytes);
            }
            ("cancel", CreationResult::Cancelled) => {
                assert!(outcome.cancelled);
                assert!(env.storage.load().unwrap().is_empty());
                assert_eq!(
                    std::fs::read(env.project_dir.join("README.md")).unwrap(),
                    b"test\n"
                );
            }
            (_, result) => panic!("{scenario}: unexpected producer outcome: {result:?}"),
        }
        poller.finish_for_test().unwrap();
    }
}

#[test]
#[serial]
fn plain_dispatch_returns_busy_and_renders_before_peer_ownership_release() {
    for layer in [
        "root-exclusive",
        "root-shared",
        "identity",
        "transition",
        "storage",
    ] {
        let mut env = setup_creation_test_env();
        let app = crate::session::get_app_dir().unwrap();
        let profile = env.storage.sessions_path().parent().unwrap().to_path_buf();
        let before = std::fs::read(env.storage.sessions_path()).ok();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let held: Box<dyn std::any::Any> = match layer {
                "root-exclusive" => {
                    Box::new(crate::session::storage::acquire_ownership_lock().unwrap())
                }
                "root-shared" => {
                    Box::new(crate::session::storage::acquire_ownership_read().unwrap())
                }
                "identity" => Box::new(
                    crate::session::storage::acquire_storage_flock(&app, ".title-mutation.lock")
                        .unwrap(),
                ),
                "transition" => Box::new(
                    crate::session::storage::acquire_storage_flock(
                        &app,
                        crate::migrations::v027_isolate_sandbox_stores::LOCK,
                    )
                    .unwrap(),
                ),
                "storage" => Box::new(
                    crate::session::storage::acquire_storage_flock(&profile, ".storage.lock")
                        .unwrap(),
                ),
                _ => unreachable!(),
            };
            ready_tx.send(()).unwrap();
            let requested_release = release_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            drop(held);
            released_tx.send(requested_release).unwrap();
        });
        ready_rx.recv().unwrap();
        let queued = env.view.creation_poller.observe_next_queued_result();
        let action = env.view.create_session_with_hooks(
            creation_data(&env.project_dir, "Busy retry", "unpublished"),
            None,
        );
        let busy_title = env
            .view
            .info_dialog
            .as_ref()
            .map(|dialog| dialog.title().to_string());
        let no_pending = !env.view.is_creation_pending() && env.view.creating_stub_id.is_none();
        env.view.handle_key(key(KeyCode::Esc), None);
        env.view.handle_key(key(KeyCode::Char('?')), None);
        let rendered_help = env.view.show_help
            && render_home_to_string(&mut env.view, 120, 35).contains("Keyboard Shortcuts");
        let unchanged = std::fs::read(env.storage.sessions_path()).ok() == before;
        let _ = release_tx.send(());
        let returned_while_held = released_rx.recv().unwrap();
        holder.join().unwrap();
        assert!(returned_while_held, "{layer}: input waited for the holder");
        assert_eq!(action, None, "{layer}");
        assert!(
            rendered_help,
            "{layer}: input and draw must complete while held"
        );
        assert!(unchanged, "{layer}: rejected admission must not write");
        assert_eq!(
            busy_title.as_deref(),
            Some("Session operation in progress"),
            "{layer}"
        );
        assert!(no_pending, "{layer}");
        assert!(env.storage.load().unwrap().is_empty(), "{layer}");
        assert!(
            queued.try_recv().is_err(),
            "{layer}: worker must not have been queued"
        );
        assert!(env.view.creating_provisional_group_paths.is_empty());
        env.view.info_dialog = None;
        env.view.show_help = false;
        env.view
            .create_session_with_hooks(creation_data(&env.project_dir, "Busy retry", ""), None);
        assert!(
            env.view.is_creation_pending(),
            "{layer}: retry should be admitted"
        );
        assert!(drain_creation_result(&mut env.view).is_some(), "{layer}");
    }
}

#[test]
#[serial]
fn completion_defers_under_every_publication_lock_then_attaches_same_identity_once() {
    use super::super::creation::PendingCreationCompletion;
    use crate::tui::creation_poller::CreationResult;
    use std::sync::mpsc;
    use std::time::Duration;

    for managed in [false, true] {
        for layer in [
            "root",
            "identity",
            "lifecycle",
            "transition",
            "storage",
            "save",
            "entry",
        ] {
            if layer == "entry" && !managed {
                continue;
            }
            let mut env = setup_creation_test_env();
            let mut data = creation_data(&env.project_dir, "Deferred completion", "deferred");
            data.worktree_enabled = managed;
            if managed {
                data.worktree_branch = Some("deferred-worktree".into());
                data.create_new_branch = true;
            }
            let queued = env.view.creation_poller.observe_next_queued_result();
            env.view.request_creation(data, None);
            let id = env.view.creating_stub_id.clone().unwrap();
            let provisional = env.view.creating_provisional_creation.clone().unwrap();
            queued.recv_timeout(Duration::from_secs(10)).unwrap();
            let outcome = env.view.creation_poller.try_recv_result().unwrap();
            let CreationResult::Success(success) = &outcome.result else {
                panic!("real build failed: {:?}", outcome.result);
            };
            let before = success.build_result.instance.clone();
            let transaction = success.build_result.transaction.clone();
            env.view.pending_creation_completion = Some(PendingCreationCompletion::Unpublished {
                outcome,
                rollback: None,
            });
            let app = crate::session::get_app_dir().unwrap();
            let profile = env.storage.sessions_path().parent().unwrap().to_path_buf();
            let storage = env.storage.clone();
            let held_id = id.clone();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (released_tx, released_rx) = mpsc::channel();
            let holder = std::thread::spawn(move || {
                let hold = || {
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(3)).is_ok()
                };
                let released = match layer {
                    "save" => storage.with_save_lock_for_test(hold),
                    "entry" => transaction.unwrap().with_entry_lock_for_test(hold),
                    _ => {
                        let held: Box<dyn std::any::Any> = match layer {
                            "root" => {
                                Box::new(crate::session::storage::acquire_ownership_lock().unwrap())
                            }
                            "identity" => Box::new(
                                crate::session::storage::acquire_storage_flock(
                                    &app,
                                    ".title-mutation.lock",
                                )
                                .unwrap(),
                            ),
                            "lifecycle" => Box::new(
                                crate::session::storage::acquire_storage_flock(
                                    &profile,
                                    &format!(".instance-lifecycle-{held_id}.lock"),
                                )
                                .unwrap(),
                            ),
                            "transition" => Box::new(
                                crate::session::storage::acquire_storage_flock(
                                    &app,
                                    crate::migrations::v027_isolate_sandbox_stores::LOCK,
                                )
                                .unwrap(),
                            ),
                            "storage" => Box::new(
                                crate::session::storage::acquire_storage_flock(
                                    &profile,
                                    ".storage.lock",
                                )
                                .unwrap(),
                            ),
                            _ => unreachable!(),
                        };
                        let released = hold();
                        drop(held);
                        released
                    }
                };
                released_tx.send(released).unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let attached = env.view.apply_creation_results();
            let retained = matches!(
                &env.view.pending_creation_completion,
                Some(PendingCreationCompletion::Unpublished { outcome, rollback: None })
                    if outcome.provisional_creation.as_ref()
                        .is_some_and(|same| std::sync::Arc::ptr_eq(same, &provisional))
            );
            let creating = env.view.get_instance(&id).unwrap().status == Status::Creating;
            env.view.handle_key(key(KeyCode::Char('?')), None);
            let rendered_help = env.view.show_help
                && render_home_to_string(&mut env.view, 120, 35).contains("Keyboard Shortcuts");
            let _ = release_tx.send(());
            let returned_while_held = released_rx.recv().unwrap();
            holder.join().unwrap();
            assert!(returned_while_held, "{managed}/{layer}: callback blocked");
            assert_eq!(attached, None, "{managed}/{layer}");
            assert!(
                retained && creating,
                "{managed}/{layer}: exact Creating owner must remain pending"
            );
            assert!(rendered_help, "{managed}/{layer}");
            env.view.show_help = false;
            assert_eq!(drain_creation_result(&mut env.view), Some(id.clone()));
            assert_eq!(
                env.view.apply_creation_results(),
                None,
                "attach is delivered once"
            );
            let rows = env.storage.load().unwrap();
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].id, id);
            assert_eq!(rows[0].created_at, before.created_at);
            assert_eq!(rows[0].lifecycle_generation, before.lifecycle_generation);
            assert_ne!(rows[0].status, Status::Creating);
            assert!(rows[0].lifecycle_reservation.is_none());
        }
    }
}

#[test]
#[serial]
fn cancelling_deferred_completion_queues_rollback_without_stealing_new_request() {
    use std::sync::mpsc;
    use std::time::Duration;
    for disconnected in [false, true] {
        let mut env = setup_creation_test_env();
        let mut data = creation_data(&env.project_dir, "Cancelled completion", "cancelled");
        data.worktree_enabled = true;
        data.worktree_branch = Some("cancelled-completion".into());
        data.create_new_branch = true;
        let queued = env.view.creation_poller.observe_next_queued_result();
        env.view.request_creation(data, None);
        let old = env.view.creating_stub_id.clone().unwrap();
        queued.recv_timeout(Duration::from_secs(10)).unwrap();
        if disconnected {
            env.view.creation_poller.disconnect_for_test();
        }
        let path = std::path::PathBuf::from(&env.storage.load().unwrap()[0].project_path);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (released_tx, released_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let root = crate::session::storage::acquire_ownership_lock().unwrap();
            ready_tx.send(()).unwrap();
            let released = release_rx.recv_timeout(Duration::from_secs(3)).is_ok();
            drop(root);
            released_tx.send(released).unwrap();
        });
        ready_rx.recv().unwrap();
        assert_eq!(env.view.apply_creation_results(), None);
        env.view.cancel_creation();
        let rolled_back = env.view.creation_poller.observe_next_queued_result();
        let hidden = env.view.get_instance(&old).is_none();
        env.view.handle_key(key(KeyCode::Esc), None);
        env.view.handle_key(key(KeyCode::Char('?')), None);
        let rendered_help = env.view.show_help
            && render_home_to_string(&mut env.view, 120, 35).contains("Keyboard Shortcuts");
        let pending = env.view.is_creation_pending();
        let _ = release_tx.send(());
        let returned_while_held = released_rx.recv().unwrap();
        holder.join().unwrap();
        assert!(returned_while_held && hidden && rendered_help && pending);
        rolled_back.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(
            !path.exists(),
            "owned worktree rollback must finish on worker"
        );
        env.view.show_help = false;
        env.view
            .request_creation(creation_data(&env.project_dir, "Newer request", ""), None);
        let new = env.view.creating_stub_id.clone().unwrap();
        assert_ne!(old, new);
        assert_eq!(
            env.view.apply_creation_results(),
            None,
            "old terminal result cannot attach"
        );
        assert_eq!(env.view.creating_stub_id.as_deref(), Some(new.as_str()));
        assert_eq!(drain_creation_result(&mut env.view), Some(new.clone()));
        assert!(env.view.get_instance(&old).is_none());
        let rows = env.storage.load().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, new);
    }
}

#[test]
#[serial]
fn published_completion_retries_projection_without_republish_cancel_or_resurrection() {
    use super::super::creation::PendingCreationCompletion;
    for deleted in [false, true] {
        let mut env = setup_creation_test_env();
        let other = Storage::new_unwatched("other").unwrap();
        other
            .update(|rows, groups| {
                rows.push(Instance::new("unrelated", "/unrelated"));
                groups.push(Group::new("unrelated", "unrelated"));
                Ok(())
            })
            .unwrap();
        env.view.storages.insert("other".into(), other.clone());
        let groups = other.sessions_path().with_file_name("groups.json");
        let backup = groups.with_extension("saved");
        std::fs::rename(&groups, &backup).unwrap();
        std::fs::create_dir(&groups).unwrap();
        let count = crate::session::config::AppStateConfig::load()
            .unwrap()
            .sessions_created;
        let queued = env.view.creation_poller.observe_next_queued_result();
        env.view.request_creation(
            creation_data(&env.project_dir, "Projection retry", ""),
            None,
        );
        let id = env.view.creating_stub_id.clone().unwrap();
        queued
            .recv_timeout(std::time::Duration::from_secs(10))
            .unwrap();
        assert_eq!(env.view.apply_creation_results(), None);
        assert!(matches!(
            env.view.pending_creation_completion,
            Some(PendingCreationCompletion::PublishedAwaitingProjection(_))
        ));
        assert_ne!(env.storage.load().unwrap()[0].status, Status::Creating);
        env.view.cancel_creation();
        assert!(
            env.view.get_instance(&id).is_some(),
            "commit won before cancellation"
        );
        assert_eq!(
            env.view.apply_creation_results(),
            None,
            "failed projection remains pending"
        );
        if deleted {
            env.storage
                .update(|rows, _| {
                    rows.retain(|row| row.id != id);
                    Ok(())
                })
                .unwrap();
        }
        std::fs::remove_dir(&groups).unwrap();
        std::fs::rename(&backup, &groups).unwrap();
        env.view.active_profile = None;
        let late = Storage::new_unwatched("undiscovered").unwrap();
        late.update(|rows, _| {
            rows.push(Instance::new("late peer", "/late-peer"));
            Ok(())
        })
        .unwrap();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let root = crate::session::storage::acquire_ownership_lock().unwrap();
            ready_tx.send(()).unwrap();
            let released = release_rx
                .recv_timeout(std::time::Duration::from_secs(3))
                .is_ok();
            drop(root);
            released_tx.send(released).unwrap();
        });
        ready_rx.recv().unwrap();
        let attached = env.view.apply_creation_results();
        env.view.handle_key(key(KeyCode::Char('?')), None);
        let rendered_help = env.view.show_help
            && render_home_to_string(&mut env.view, 120, 35).contains("Keyboard Shortcuts");
        let _ = release_tx.send(());
        let returned_while_held = released_rx.recv().unwrap();
        holder.join().unwrap();
        assert!(
            returned_while_held && rendered_help,
            "projection must use only bound read storage"
        );
        assert!(!env.view.storages.contains_key("undiscovered"));
        assert_eq!(attached, (!deleted).then_some(id.clone()));
        assert_eq!(env.view.apply_creation_results(), None);
        assert!(!env.view.is_creation_pending());
        assert_eq!(
            crate::session::config::AppStateConfig::load()
                .unwrap()
                .sessions_created,
            count + 1
        );
        assert_eq!(env.view.get_instance(&id).is_none(), deleted);
        env.view.save().unwrap();
        assert_eq!(env.storage.load().unwrap().is_empty(), deleted);
    }
}
