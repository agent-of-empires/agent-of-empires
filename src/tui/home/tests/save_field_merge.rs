use super::*;
use crate::session::Status;
use chrono::Utc;

fn boot_view_with_one_session(title: &str, path: &str) -> (TempDir, AppDirGuard, HomeView, String) {
    // The daemon-ownership purge path reads the pending-purge journal during
    // teardown (`protection`), which real startup creates via migration. Seed
    // it here so the queued deletion can complete once the flock releases.
    let temp = TempDir::new().unwrap();
    let guard = setup_test_home(&temp);
    crate::session::purge_owners::initialize(&crate::session::get_app_dir().unwrap()).unwrap();
    let storage = Storage::new_unwatched("test").unwrap();
    let inst = Instance::new(title, path);
    let id = inst.id.clone();
    storage
        .update(|i, g| {
            i.push(inst.clone());
            *g = GroupTree::new_with_groups(&[inst], &[]).get_all_groups();
            Ok(())
        })
        .unwrap();

    let tools = AvailableTools::with_tools(&["claude"]);
    let view = HomeView::new_for_test(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    (temp, guard, view, id)
}
#[test]
#[serial]
fn delete_action_does_not_wait_for_lifecycle_flock() {
    use crate::tui::dialogs::DeleteOptions;

    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/delete-lock");
    view.selected_session = Some(id.clone());
    let storage = Storage::new_unwatched("test").unwrap();
    let lifecycle_lock = storage.acquire_instance_lifecycle_lock(&id).unwrap();

    // Returning with the flock still held proves this action only enqueues.
    view.delete_selected(&DeleteOptions::default()).unwrap();
    assert_eq!(
        view.get_instance(&id).map(|instance| instance.status),
        Some(crate::session::Status::Deleting)
    );
    drop(lifecycle_lock);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !view.apply_deletion_results() {
        assert!(
            std::time::Instant::now() < deadline,
            "deletion did not complete"
        );
        std::thread::yield_now();
    }
    assert!(
        view.get_instance(&id).is_none(),
        "queued deletion removed the row"
    );
}

/// A TUI save with a stale view merges peer writes (a field, a new row, a new group) instead
/// of clobbering them.
#[test]
#[serial]
fn test_save_preserves_peer_writes() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("a", "/tmp/a");

    let peer_archived_at = Utc::now();
    Storage::new_unwatched("test")
        .unwrap()
        .update(|insts, groups| {
            if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                inst.archived_at = Some(peer_archived_at);
            }
            insts.push(Instance::new("peer-added", "/tmp/peer"));
            groups.push(crate::session::Group::new("peer-grp", "peer-grp"));
            Ok(())
        })
        .unwrap();

    view.save().expect("save must merge peer writes");

    let (reloaded, groups) = Storage::new_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert_eq!(row.archived_at, Some(peer_archived_at), "peer field write");
    assert!(reloaded.iter().any(|i| i.title == "peer-added"), "peer row");
    assert!(groups.iter().any(|g| g.path == "peer-grp"), "peer group");
}

#[test]
#[serial]
fn test_save_drops_explicitly_deleted_row() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/victim");

    view.remove_instance(&id);
    assert!(
        view.pending_deletions
            .get("test")
            .is_some_and(|s| s.contains(&id)),
        "remove_instance must populate pending_deletions"
    );
    view.save().expect("save must propagate the delete");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(
        !reloaded.iter().any(|i| i.id == id),
        "tombstoned row must be removed from disk"
    );
    assert!(
        !view.pending_deletions.contains_key("test"),
        "pending_deletions must drain on Ok save"
    );
}

/// `apply_user_action` persists its own edit with a field-level merge, so unrelated peer
/// writes survive.
#[test]
#[serial]
fn test_apply_user_action_does_not_clobber_peer_field() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

    let peer_storage = Storage::new_unwatched("test").unwrap();
    peer_storage
        .update(|insts, _| {
            if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                inst.notify_on_waiting = Some(true);
                inst.group_path = "peer/group".to_string();
            }
            Ok(())
        })
        .unwrap();

    view.apply_user_action(&id, |inst| inst.archive())
        .expect("archive must persist");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert!(row.archived_at.is_some(), "TUI archive landed");
    assert_eq!(row.notify_on_waiting, Some(true));
    assert_eq!(row.group_path, "peer/group");
}

#[test]
#[serial]
fn test_apply_user_action_disk_and_memory_share_one_timestamp() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

    view.apply_user_action(&id, |inst| inst.archive())
        .expect("apply_user_action must persist");

    let mem_ts = view
        .get_instance(&id)
        .expect("in-memory row present")
        .archived_at;
    let disk_ts = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|i| i.id == id)
        .expect("disk row present")
        .archived_at;
    assert_eq!(
        mem_ts, disk_ts,
        "single Utc::now() snapshot, no microsecond drift between memory and disk"
    );
}

#[test]
#[serial]
fn test_apply_user_action_archive_clears_peer_snooze() {
    // The web/TUI/CLI contract treats pinned / archived / snoozed as mutually exclusive (see
    // Instance::archive and the tier comparator in #1581), so when a peer snoozes a row the
    // TUI then archives, archive wins as the indefinite sink; both flags would surface
    // contradictory triage state.
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

    let peer_storage = Storage::new_unwatched("test").unwrap();
    peer_storage
        .update(|insts, _| {
            if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                inst.snooze(30);
            }
            Ok(())
        })
        .unwrap();

    view.apply_user_action(&id, |inst| inst.archive())
        .expect("archive must persist");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert!(row.archived_at.is_some(), "TUI archive landed");
    assert!(
        row.snoozed_until.is_none(),
        "archive() invariant must clear a concurrent peer snooze",
    );
}

#[test]
#[serial]
fn test_save_drops_peer_deleted_row_from_mirror() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/peer-rm");

    // Simulate `aoe session remove victim` from another process: peer
    // deletes the row from disk while TUI still has it in memory.
    Storage::new_unwatched("test")
        .unwrap()
        .update(|insts, _g| {
            insts.retain(|i| i.id != id);
            Ok(())
        })
        .unwrap();

    view.save()
        .expect("save must not error on peer-deleted rows");

    assert!(
        !view.instances().any(|i| i.id == id),
        "peer-deleted row must be dropped from in-memory instances"
    );
    assert!(
        view.get_instance(&id).is_none(),
        "peer-deleted row must be dropped from in-memory mirror"
    );
    let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(
        !disk.iter().any(|i| i.id == id),
        "save() must not resurrect the peer-deleted row on disk"
    );
}

/// A TUI-added row is pushed to disk; one added and removed in the same cycle never is.
#[test]
#[serial]
fn test_save_pushes_tui_added_row_to_disk() {
    let (_temp, _guard, mut view, _) = boot_view_with_one_session("seed", "/tmp/seed");

    let mut added = Instance::new("tui-added", "/tmp/added");
    added.source_profile = "test".to_string();
    let added_id = added.id.clone();
    view.add_instance(added);
    let mut ephemeral = Instance::new("ephemeral", "/tmp/ephemeral");
    ephemeral.source_profile = "test".to_string();
    let ephemeral_id = ephemeral.id.clone();
    view.add_instance(ephemeral);
    view.remove_instance(&ephemeral_id);

    view.save().expect("save must persist TUI-added row");

    let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(disk.iter().any(|i| i.id == added_id));
    assert!(!disk.iter().any(|i| i.id == ephemeral_id));
    assert!(
        !view.pending_added.contains_key("test"),
        "pending_added must drain on Ok save"
    );
}

#[test]
#[serial]
fn test_move_to_profile_commits_without_pending_bookkeeping() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");
    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );
    let runtime_sentinel = std::time::Instant::now();
    view.mutate_instance(&id, |instance| {
        instance.last_error = Some("live-only-error".to_string());
        instance.last_error_check = Some(runtime_sentinel);
        instance.last_start_time = Some(runtime_sentinel);
        instance.live_status_baseline = Some(Status::Waiting);
        instance.ever_confirmed_present = true;
        instance.unknown_since = Some(runtime_sentinel);
        instance.pane_dead_observed = true;
        instance.force_fresh_next_launch = true;
    });

    let mut requested = view.get_instance(&id).unwrap().clone();
    requested.group_path = "moved/group".to_string();
    view.move_to_profile_with_effect(&id, "target", requested, None, false, |_| Ok(()))
        .unwrap();
    view.reload_preserving_profile_move_runtime(std::slice::from_ref(&id))
        .unwrap();

    assert!(!view.pending_deletions.values().any(|ids| ids.contains(&id)));
    assert!(!view.pending_added.values().any(|ids| ids.contains(&id)));
    let source = Storage::new_unwatched("test").unwrap().load().unwrap();
    let target = Storage::new_unwatched("target").unwrap().load().unwrap();
    assert!(!source.iter().any(|instance| instance.id == id));
    let moved = target
        .iter()
        .find(|instance| instance.id == id)
        .expect("target row committed");
    assert_eq!(moved.group_path, "moved/group");
    let in_memory = view.get_instance(&id).unwrap();
    assert_eq!(in_memory.source_profile, "target");
    assert_eq!(in_memory.group_path, "moved/group");
    assert_eq!(
        in_memory.last_error.as_deref(),
        Some("live-only-error"),
        "runtime-only error must survive publication"
    );
    assert_eq!(in_memory.last_error_check, Some(runtime_sentinel));
    assert_eq!(in_memory.last_start_time, Some(runtime_sentinel));
    assert_eq!(in_memory.live_status_baseline, Some(Status::Waiting));
    assert!(in_memory.ever_confirmed_present);
    assert_eq!(in_memory.unknown_since, Some(runtime_sentinel));
    assert!(in_memory.pane_dead_observed);
    assert!(in_memory.force_fresh_next_launch);
}

/// A same-profile move only regroups (no tombstone); a cross-profile move then saves the row
/// under the target profile only.
#[test]
#[serial]
fn test_move_to_profile_save_roundtrip_persists_under_target() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");

    let mut requested = view.get_instance(&id).unwrap().clone();
    requested.group_path = "newgrp".to_string();
    view.move_to_profile_with_effect(&id, "test", requested, None, false, |_| Ok(()))
        .unwrap();
    assert!(
        !view
            .pending_deletions
            .get("test")
            .is_some_and(|ids| ids.contains(&id)),
        "same-profile move must NOT tombstone the row"
    );
    assert_eq!(view.get_instance(&id).unwrap().group_path, "newgrp");

    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );
    let mut requested = view.get_instance(&id).unwrap().clone();
    requested.group_path.clear();
    view.move_to_profile_with_effect(&id, "target", requested, None, false, |_| Ok(()))
        .unwrap();
    view.save().expect("save must succeed across profiles");

    let old_disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    let new_disk = Storage::new_unwatched("target").unwrap().load().unwrap();
    assert!(
        !old_disk.iter().any(|i| i.id == id),
        "old profile disk must NOT contain the moved row"
    );
    assert!(
        new_disk.iter().any(|i| i.id == id),
        "new profile disk MUST contain the moved row"
    );
}

#[test]
#[serial]
fn group_profile_move_reloads_members_and_registers_fallback_source() {
    let temp = TempDir::new().unwrap();
    let _guard = setup_test_home(&temp);
    let source = Storage::new_unwatched("alpha").unwrap();
    let mut row = Instance::new("stale-title", "/tmp/group-profile-authority");
    row.group_path = "work".to_string();
    let id = row.id.clone();
    source
        .update(|instances, groups| {
            instances.push(row);
            groups.push(crate::session::Group::new("work", "work"));
            Ok(())
        })
        .unwrap();
    let target = Storage::new_unwatched("beta").unwrap();
    let tools = AvailableTools::with_tools(&["claude"]);
    let mut view =
        HomeView::new_for_test(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
    view.mutate_instance(&id, |instance| {
        instance.title = "stale-memory-title".to_string();
        instance.lifecycle_generation = 3;
    });
    source
        .update(|instances, _groups| {
            let authoritative = instances.iter_mut().find(|row| row.id == id).unwrap();
            authoritative.title = "peer-title".to_string();
            authoritative.lifecycle_generation = 7;
            Ok(())
        })
        .unwrap();
    view.storages.remove("alpha");
    view.group_rename_context = Some(super::super::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });

    view.rename_selected_group(None, Some("beta")).unwrap();

    assert!(source.load().unwrap().is_empty());
    let moved = target
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .expect("authoritative row must move to the target");
    assert_eq!(moved.title, "peer-title");
    assert_eq!(moved.lifecycle_generation, 7);
}

#[test]
#[serial]
fn profile_only_move_seeds_target_from_authoritative_title_and_lifecycle() {
    let (_temp, _guard, mut view, id) =
        boot_view_with_one_session("stale-title", "/tmp/profile-authority");
    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );

    view.mutate_instance(&id, |row| {
        row.lifecycle_generation = 3;
        row.status = Status::Idle;
    });
    let source = Storage::new_unwatched("test").unwrap();
    source
        .update(|instances, _groups| {
            let row = instances.iter_mut().find(|row| row.id == id).unwrap();
            row.title = "peer-new-title".to_string();
            row.lifecycle_generation = 7;
            row.status = Status::Running;
            Ok(())
        })
        .unwrap();

    let _guards = view.lock_session_mutation_and_reload(&id).unwrap();
    let authoritative = view.get_instance(&id).cloned().unwrap();
    assert_eq!(authoritative.title, "peer-new-title");
    assert_eq!(authoritative.lifecycle_generation, 7);
    assert_eq!(authoritative.status, Status::Running);

    let requested = authoritative.clone();
    view.move_to_profile_with_effect(&id, "target", requested, None, false, |_| Ok(()))
        .unwrap();
    view.save().unwrap();

    let target = Storage::new_unwatched("target")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .unwrap();
    assert_eq!(target.title, "peer-new-title");
    assert_eq!(target.lifecycle_generation, 7);
    assert_eq!(target.status, Status::Running);
}

#[test]
#[serial]
fn profile_move_blocks_fresh_but_allows_stale_lifecycle_reservation() {
    let (_temp, _guard, mut view, id) =
        boot_view_with_one_session("reserved", "/tmp/profile-reserved");
    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );
    let reservation = LifecycleReservation {
        op: LifecycleOperation::Launch,
        generation: 1,
        at: chrono::Utc::now(),
    };
    view.mutate_instance(&id, |row| {
        row.lifecycle_generation = 1;
        row.lifecycle_reservation = Some(reservation.clone());
        row.status = Status::Starting;
    });
    let source = Storage::new_unwatched("test").unwrap();
    source
        .update(|instances, _groups| {
            let row = instances.iter_mut().find(|row| row.id == id).unwrap();
            row.lifecycle_generation = 1;
            row.lifecycle_reservation = Some(reservation);
            row.status = Status::Starting;
            Ok(())
        })
        .unwrap();

    let _guards = view.lock_session_mutation_and_reload(&id).unwrap();
    let requested = view.get_instance(&id).cloned().unwrap();
    let error = view
        .move_to_profile_with_effect(&id, "target", requested, None, false, |_| Ok(()))
        .expect_err("reserved session must not move profiles");

    assert!(error
        .to_string()
        .contains("lifecycle operation is in progress"));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
    assert!(view
        .pending_deletions
        .get("test")
        .is_none_or(|ids| !ids.contains(&id)));
    assert!(Storage::new_unwatched("target")
        .unwrap()
        .load()
        .unwrap()
        .is_empty());
    assert!(source.load().unwrap().iter().any(|row| row.id == id));

    let stale_at =
        chrono::Utc::now() - Instance::LIFECYCLE_RESERVATION_TTL - chrono::Duration::seconds(1);
    view.mutate_instance(&id, |row| {
        row.lifecycle_reservation.as_mut().unwrap().at = stale_at;
    });
    source
        .update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|row| row.id == id)
                .unwrap()
                .lifecycle_reservation
                .as_mut()
                .unwrap()
                .at = stale_at;
            Ok(())
        })
        .unwrap();

    let requested = view.get_instance(&id).unwrap().clone();
    let baseline = requested.clone();
    view.move_to_profile_with_effect(&id, "target", requested, Some(&baseline), false, |_| Ok(()))
        .expect("stale reservation must not block profile move");
    assert!(source.load().unwrap().is_empty());
    assert!(Storage::new_unwatched("target")
        .unwrap()
        .load()
        .unwrap()
        .iter()
        .any(|row| row.id == id));
}

#[test]
#[serial]
fn test_reload_honors_peer_cleared_session_id() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/sid");

    // Seed a stale sid via the in-memory mirror + persist.
    view.mutate_instance(&id, |inst| {
        inst.agent_session_id = Some("stale_X".to_string());
    });
    view.save().unwrap();

    // Peer clears the sid on disk (simulates `aoe session set-session-id ""`).
    Storage::new_unwatched("test")
        .unwrap()
        .update(|insts, _g| {
            if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                inst.agent_session_id = None;
            }
            Ok(())
        })
        .unwrap();

    view.reload().unwrap();

    assert!(
        view.get_instance(&id)
            .and_then(|i| i.agent_session_id.clone())
            .is_none(),
        "reload must honor peer-cleared sid; carrying memory would re-pass --resume <stale>"
    );
}

/// Engaging either sunk state sends one daemon mutation; the disk only changes
/// once the server commits and publishes its receipt.
#[test]
#[serial]
fn engaging_sunk_rows_requests_canonical_access_without_local_write() {
    for archived in [true, false] {
        let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");
        view.apply_user_action(&id, |row| {
            if archived {
                row.archive();
            } else {
                row.snooze(30);
            }
        })
        .unwrap();
        let mut respond = view.session_feed.command_driver_for_test();
        view.stamp_last_accessed(&id);
        let (_, mutation) = respond(Ok(crate::daemon::RuntimeCursor {
            epoch: "test".into(),
            revision: 2,
        }))
        .unwrap();
        assert!(matches!(mutation, crate::daemon::SessionMutation::Access));
        let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
        let stored = disk.iter().find(|row| row.id == id).unwrap();
        assert_eq!(stored.is_archived(), archived);
        assert_eq!(stored.snoozed_until.is_some(), !archived);
    }
}
#[test]
#[serial]
fn refused_restart_preserves_launch_fields_and_snooze_in_memory_and_on_disk() {
    let (_temp, _guard, mut view, id) =
        boot_view_with_one_session("victim", "/tmp/restart-refusal");
    let snoozed_until = Utc::now() + chrono::Duration::minutes(30);
    let seed = |instance: &mut Instance| {
        instance.tool = "claude".into();
        instance.command = "original-wrapper".into();
        instance.extra_args = "--original".into();
        instance.agent_session_id = Some("source-durable-sid".into());
        instance.snoozed_until = Some(snoozed_until);
        instance.status = Status::Running;
    };
    view.mutate_instance(&id, seed);
    view.storages["test"]
        .update(|instances, _groups| {
            seed(instances.iter_mut().find(|row| row.id == id).unwrap());
            Ok(())
        })
        .unwrap();
    view.selected_session = Some(id.clone());
    view.sort_order = crate::session::config::SortOrder::Newest;

    // No command driver: the disconnected feed must refuse before any edits.
    let result =
        view.restart_selected_session(None, Some("codex"), Some("--new"), Some("codex-wrapper"));
    assert!(result.is_err() || view.info_dialog.is_some());

    let disk = view.storages["test"].load().unwrap();
    let stored = disk.iter().find(|row| row.id == id).unwrap();
    for row in [stored, view.get_instance(&id).unwrap()] {
        assert_eq!(row.tool, "claude");
        assert_eq!(row.command, "original-wrapper");
        assert_eq!(row.extra_args, "--original");
        assert_eq!(row.snoozed_until, Some(snoozed_until));
        assert_eq!(row.agent_session_id.as_deref(), Some("source-durable-sid"));
        assert!(row.prior_tool_session_ids.is_empty());
    }
    assert_eq!(view.get_instance(&id).unwrap().status, Status::Running);
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
    assert!(!view.restart_in_flight.contains(&id));
    assert!(!view.restart_cooldown_at.contains_key(&id));
}

#[test]
#[serial]
fn rename_profile_move_validates_complete_candidate_before_commit() {
    let (_temp, _guard, mut view, id) =
        boot_view_with_one_session("old-name", "/tmp/profile-rename");
    let target = Storage::new_unwatched("target").unwrap();
    target
        .update(|instances, groups| {
            let mut collision = Instance::new("new-name", "/tmp/profile-rename/");
            collision.source_profile = "target".to_string();
            instances.push(collision);
            groups.push(Group::new("existing", "existing"));
            Ok(())
        })
        .unwrap();
    view.storages.insert("target".to_string(), target);
    view.selected_session = Some(id.clone());

    let result = view.rename_selected("new-name", Some("renamed/group"), Some("target"), false);
    assert!(result.is_err());

    let (source_rows, source_groups) = Storage::new_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    let source = source_rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(source.title, "old-name");
    assert!(source.group_path.is_empty());
    assert!(source_groups.is_empty());
    let (target_rows, target_groups) = Storage::new_unwatched("target")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert_eq!(target_rows.len(), 1);
    assert_eq!(target_groups.len(), 1);
    assert_eq!(target_groups[0].path, "existing");
    let in_memory = view.get_instance(&id).unwrap();
    assert_eq!(in_memory.source_profile, "test");
    assert_eq!(in_memory.title, "old-name");
}

#[test]
#[serial]
fn tied_cross_profile_collision_rejects_before_worktree_effects() {
    let temp = TempDir::new().unwrap();
    let old_path = temp.path().join("old-name");
    let new_path = temp.path().join("new-name");
    std::fs::create_dir_all(&old_path).unwrap();
    std::fs::write(old_path.join("sentinel"), b"untouched").unwrap();
    let (_home, _guard, mut view, id) =
        boot_view_with_one_session("old-name", old_path.to_str().unwrap());
    let worktree = crate::session::WorktreeInfo {
        branch: "old-name".to_string(),
        main_repo_path: temp
            .path()
            .join("missing-repo")
            .to_string_lossy()
            .to_string(),
        managed_by_aoe: true,
        created_at: chrono::Utc::now(),
        base_branch: None,
    };
    view.mutate_instance(&id, |instance| {
        instance.worktree_info = Some(worktree.clone());
        instance.status = Status::Stopped;
    });
    view.storages["test"]
        .update(|instances, _groups| {
            let source = instances.iter_mut().find(|row| row.id == id).unwrap();
            source.worktree_info = Some(worktree.clone());
            source.status = Status::Stopped;
            Ok(())
        })
        .unwrap();
    let target = Storage::new_unwatched("target").unwrap();
    target
        .update(|instances, _groups| {
            instances.push(Instance::new(
                "new-name",
                new_path.to_string_lossy().as_ref(),
            ));
            Ok(())
        })
        .unwrap();
    view.storages.insert("target".to_string(), target);
    view.selected_session = Some(id.clone());

    let result = view.rename_selected("new-name", None, Some("target"), true);

    assert!(result.is_err());
    assert!(old_path.exists());
    assert_eq!(
        std::fs::read(old_path.join("sentinel")).unwrap(),
        b"untouched"
    );
    assert!(
        !new_path.exists(),
        "no target directory may be created before validation"
    );
    let source = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .unwrap();
    assert_eq!(source.title, "old-name");
    assert_eq!(source.project_path, old_path.to_string_lossy().to_string());
    assert_eq!(source.worktree_info.unwrap().branch, "old-name");
}
