use super::*;
use crate::session::Status;
use chrono::Utc;

fn boot_view_with_one_session(title: &str, path: &str) -> (TempDir, AppDirGuard, HomeView, String) {
    let temp = TempDir::new().unwrap();
    let guard = setup_test_home(&temp);
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
    let view = HomeView::new(
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

    let started = std::time::Instant::now();
    view.delete_selected(&DeleteOptions::default()).unwrap();
    assert!(
        started.elapsed() < std::time::Duration::from_millis(100),
        "the event-loop action must only enqueue deletion"
    );
    assert_eq!(
        view.get_instance(&id).map(|instance| instance.status),
        Some(crate::session::Status::Deleting)
    );
    drop(lifecycle_lock);
}

#[test]
#[serial]
fn test_save_preserves_peer_field_update() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

    let peer_storage = Storage::new_unwatched("test").unwrap();
    let peer_archived_at = Utc::now();
    peer_storage
        .update(|insts, _| {
            if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                inst.archived_at = Some(peer_archived_at);
            }
            Ok(())
        })
        .unwrap();

    view.save().expect("save must merge peer-owned field write");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert_eq!(
        row.archived_at,
        Some(peer_archived_at),
        "peer's archive must survive a TUI save with stale view"
    );
}

#[test]
#[serial]
fn test_save_preserves_peer_added_row() {
    let (_temp, _guard, mut view, _id) = boot_view_with_one_session("a", "/tmp/a");

    let peer_storage = Storage::new_unwatched("test").unwrap();
    peer_storage
        .update(|insts, _| {
            insts.push(Instance::new("peer-added", "/tmp/peer"));
            Ok(())
        })
        .unwrap();

    view.save()
        .expect("save must not delete rows the TUI does not know about");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(
        reloaded.iter().any(|i| i.title == "peer-added"),
        "peer-added row must survive TUI save"
    );
    assert!(
        reloaded.iter().any(|i| i.title == "a"),
        "TUI's known row must remain"
    );
}

#[test]
#[serial]
fn test_save_drops_explicitly_deleted_row() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/victim");

    view.remove_instance(&id);
    view.save().expect("save must propagate the delete");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(
        !reloaded.iter().any(|i| i.id == id),
        "tombstoned row must be removed from disk"
    );
}

#[test]
#[serial]
fn test_save_drains_pending_deletions_on_ok() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/victim");

    view.remove_instance(&id);
    assert!(
        view.pending_deletions
            .get("test")
            .is_some_and(|s| s.contains(&id)),
        "remove_instance must populate pending_deletions"
    );

    view.save().unwrap();

    assert!(
        !view.pending_deletions.contains_key("test"),
        "pending_deletions must drain on Ok save"
    );
}

#[test]
#[serial]
fn test_save_preserves_peer_added_group() {
    let (_temp, _guard, mut view, _id) = boot_view_with_one_session("a", "/tmp/a");

    let peer_storage = Storage::new_unwatched("test").unwrap();
    peer_storage
        .update(|_insts, groups| {
            groups.push(crate::session::Group::new("peer-grp", "peer-grp"));
            Ok(())
        })
        .unwrap();

    view.save()
        .expect("save must not clobber groups the TUI does not know about");

    let reloaded = Storage::new_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap()
        .1;
    assert!(
        reloaded.iter().any(|g| g.path == "peer-grp"),
        "peer-added group must survive TUI save"
    );
}

#[test]
#[serial]
fn test_apply_user_action_persists_atomically() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

    view.apply_user_action(&id, |inst| inst.archive())
        .expect("apply_user_action must persist");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert!(
        row.archived_at.is_some(),
        "apply_user_action must persist archived_at to disk"
    );
}

#[test]
#[serial]
fn test_apply_user_action_does_not_clobber_peer_field() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

    let peer_storage = Storage::new_unwatched("test").unwrap();
    peer_storage
        .update(|insts, _| {
            if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                inst.notify_on_waiting = Some(true);
            }
            Ok(())
        })
        .unwrap();

    view.apply_user_action(&id, |inst| inst.archive())
        .expect("archive must persist");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert!(row.archived_at.is_some(), "TUI archive landed");
    assert_eq!(
        row.notify_on_waiting,
        Some(true),
        "peer's notify_on_waiting must survive an apply_user_action that does not touch it"
    );
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
    // The web/TUI/CLI contract treats pinned / archived / snoozed
    // as mutually exclusive (see Instance::archive and the sidebar
    // tier comparator in #1581). When a peer snoozes a row that
    // the TUI then archives, archive wins because it is the
    // indefinite sink; leaving both flags persisted would surface
    // contradictory triage state on the next render.
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
fn test_apply_user_action_preserves_peer_user_action_field() {
    // Field-level merge regression: a TUI snooze must not clobber
    // an unrelated peer write (group_path here). Uses snooze
    // instead of archive so the snoozed_until field IS touched on
    // both sides and the test isolates the peer-field-survival
    // invariant from the archive XOR rules tested above.
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/race");

    let peer_storage = Storage::new_unwatched("test").unwrap();
    peer_storage
        .update(|insts, _| {
            if let Some(inst) = insts.iter_mut().find(|i| i.id == id) {
                inst.group_path = "peer/group".to_string();
            }
            Ok(())
        })
        .unwrap();

    view.apply_user_action(&id, |inst| inst.snooze(30))
        .expect("snooze must persist");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    let row = reloaded.iter().find(|i| i.id == id).expect("row present");
    assert!(row.snoozed_until.is_some(), "TUI snooze landed");
    assert_eq!(
        row.group_path, "peer/group",
        "peer-written group_path must survive a TUI snooze that does not touch the field",
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

#[test]
#[serial]
fn test_save_pushes_tui_added_row_to_disk() {
    let (_temp, _guard, mut view, _) = boot_view_with_one_session("seed", "/tmp/seed");

    let mut new_inst = Instance::new("tui-added", "/tmp/added");
    new_inst.source_profile = "test".to_string();
    let new_id = new_inst.id.clone();
    view.add_instance(new_inst);

    view.save().expect("save must persist TUI-added row");

    let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(
        disk.iter().any(|i| i.id == new_id),
        "TUI-added row must be persisted to disk"
    );
    assert!(
        !view.pending_added.contains_key("test"),
        "pending_added must drain on Ok save"
    );
}

#[test]
#[serial]
fn test_save_add_then_remove_in_same_cycle_does_not_persist() {
    let (_temp, _guard, mut view, _) = boot_view_with_one_session("seed", "/tmp/seed");

    let mut new_inst = Instance::new("ephemeral", "/tmp/ephemeral");
    new_inst.source_profile = "test".to_string();
    let new_id = new_inst.id.clone();
    view.add_instance(new_inst);
    view.remove_instance(&new_id);

    view.save().expect("save must succeed");

    let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(
        !disk.iter().any(|i| i.id == new_id),
        "add+remove in same save cycle must not leak the row to disk"
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
    view.move_to_profile(&id, "target", requested, None)
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

#[test]
#[serial]
fn test_move_to_profile_save_roundtrip_persists_under_target() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");
    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );

    let mut requested = view.get_instance(&id).unwrap().clone();
    requested.group_path.clear();
    view.move_to_profile(&id, "target", requested, None)
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
fn restart_profile_move_rejects_target_identity_collision_before_mutation() {
    let (_temp, _guard, mut view, id) =
        boot_view_with_one_session("source", "/tmp/profile-restart-collision");
    let target = Storage::new_unwatched("target").unwrap();
    target
        .update(|instances, _groups| {
            let mut collision = Instance::new("source", "/tmp/profile-restart-collision/");
            collision.source_profile = "target".to_string();
            instances.push(collision);
            Ok(())
        })
        .unwrap();
    view.storages.insert("target".to_string(), target);
    view.selected_session = Some(id.clone());

    let error = view
        .restart_selected_session(Some("target"), Some("claude"), None, None)
        .expect_err("target identity collision must reject restart profile move");

    assert!(error
        .to_string()
        .contains("Session already exists with same title and path"));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
    assert!(!view.restart_cooldown_at.contains_key(&id));
    assert_eq!(
        Storage::new_unwatched("test")
            .unwrap()
            .load()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        Storage::new_unwatched("target")
            .unwrap()
            .load()
            .unwrap()
            .len(),
        1
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
    let mut view = HomeView::new(None, tools, crate::file_watch::FileWatchService::noop()).unwrap();
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
    view.move_to_profile(&id, "target", requested, None)
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
        .move_to_profile(&id, "target", requested, None)
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
    view.move_to_profile(&id, "target", requested, Some(&baseline))
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
fn restart_profile_move_rejects_invalid_targets_before_mutation() {
    let (_temp, _guard, mut view, id) =
        boot_view_with_one_session("source", "/tmp/profile-restart-collision");
    view.selected_session = Some(id.clone());

    let error = view
        .restart_selected_session(Some("missing-target"), Some("claude"), None, None)
        .expect_err("missing target profile must reject restart profile move");
    assert!(error
        .to_string()
        .contains("Profile 'missing-target' does not exist"));
    assert!(!crate::session::list_profiles()
        .unwrap()
        .contains(&"missing-target".to_string()));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
    assert!(!view.restart_cooldown_at.contains_key(&id));

    let target = Storage::new_unwatched("target").unwrap();
    target
        .update(|instances, _groups| {
            let mut collision = Instance::new("source", "/tmp/profile-restart-collision/");
            collision.source_profile = "target".to_string();
            instances.push(collision);
            Ok(())
        })
        .unwrap();
    view.storages.insert("target".to_string(), target);

    let error = view
        .restart_selected_session(Some("target"), Some("claude"), None, None)
        .expect_err("target identity collision must reject restart profile move");

    assert!(error
        .to_string()
        .contains("Session already exists with same title and path"));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
    assert!(!view.restart_cooldown_at.contains_key(&id));
    assert_eq!(
        Storage::new_unwatched("test")
            .unwrap()
            .load()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        Storage::new_unwatched("target")
            .unwrap()
            .load()
            .unwrap()
            .len(),
        1
    );
}

#[test]
#[serial]
fn test_move_to_profile_same_profile_only_updates_group_path() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/move");

    let mut requested = view.get_instance(&id).unwrap().clone();
    requested.group_path = "newgrp".to_string();
    view.move_to_profile(&id, "test", requested, None).unwrap();

    assert!(
        !view.pending_deletions.contains_key("test")
            || !view.pending_deletions.get("test").unwrap().contains(&id),
        "same-profile move must NOT tombstone the row"
    );
    assert_eq!(view.get_instance(&id).unwrap().group_path, "newgrp");
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

/// `stamp_last_accessed` on a sunk row must auto-clear archived_at on
/// BOTH memory and disk, and rebuild flat_items so the row leaves the
/// synthetic Archived section on the same frame. Regression guard for
/// the "re-entering an archived session left it stuck in the Archived
/// section until the user pressed `z`" bug: the old implementation used
/// mutate_instance + save, but merge_from_tui doesn't carry archived_at
/// so the next reload resurrected the sink from disk.
#[test]
#[serial]
fn stamp_last_accessed_on_archived_row_unsinks_persistently() {
    use crate::session::{is_archived_section_path, Item};

    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");

    view.apply_user_action(&id, |inst| inst.archive())
        .expect("seed archive must persist");
    view.flat_items = view.build_flat_items();
    assert!(
        view.get_instance(&id).unwrap().is_archived(),
        "precondition: row archived in memory"
    );
    let archived_section_present = |items: &[Item]| {
        items.iter().any(|it| match it {
            Item::Group { path, .. } => is_archived_section_path(path),
            _ => false,
        })
    };

    assert!(
        archived_section_present(&view.flat_items),
        "precondition: Archived section header rendered"
    );

    view.stamp_last_accessed(&id);

    assert!(
        !view.get_instance(&id).unwrap().is_archived(),
        "stamp_last_accessed must clear archived_at in memory"
    );
    let disk_row = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|i| i.id == id)
        .expect("disk row present");
    assert!(
        disk_row.archived_at.is_none(),
        "stamp_last_accessed must persist the auto-unarchive (merge_from_tui drops archived_at)"
    );
    assert!(
        !archived_section_present(&view.flat_items),
        "Archived section must disappear once the only archived row is unsunk"
    );
}
#[test]
#[serial]
fn restart_profile_move_commits_staged_launch_edit() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/profile-launch");
    fn seed_swap_state(instance: &mut Instance) {
        instance.tool = "claude".to_string();
        instance.agent_session_id = Some("claude-session".to_string());
    }
    view.mutate_instance(&id, seed_swap_state);
    view.storages["test"]
        .update(|instances, _groups| {
            seed_swap_state(instances.iter_mut().find(|row| row.id == id).unwrap());
            Ok(())
        })
        .unwrap();
    view.storages["test"]
        .update(|instances, _groups| {
            let fresh = instances.iter_mut().find(|row| row.id == id).unwrap();
            fresh.agent_session_id = Some("fresh-claude-session".to_string());
            Ok(())
        })
        .unwrap();
    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );
    view.selected_session = Some(id.clone());

    view.restart_selected_session(
        Some("target"),
        Some("codex"),
        Some("--fast"),
        Some("codex-wrapper"),
    )
    .unwrap();

    let (source_rows, _) = Storage::new_unwatched("test")
        .unwrap()
        .load_with_groups()
        .unwrap();
    assert!(!source_rows.iter().any(|row| row.id == id));
    let target_rows = Storage::new_unwatched("target").unwrap().load().unwrap();
    let moved = target_rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(moved.tool, "codex");
    assert_eq!(moved.command, "codex-wrapper");
    assert_eq!(moved.extra_args, "--fast");
    assert_eq!(moved.agent_session_id, None);
    assert_eq!(
        moved.prior_tool_session_ids["claude"]
            .agent_session_id
            .as_deref(),
        Some("fresh-claude-session")
    );
}

#[test]
#[serial]
fn restart_profile_move_rejection_leaves_source_tool_state_unchanged() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("victim", "/tmp/profile-reject");
    view.mutate_instance(&id, |instance| {
        instance.tool = "claude".to_string();
        instance.agent_session_id = Some("source-durable-sid".to_string());
    });
    view.storages["test"]
        .update(|instances, _groups| {
            let source = instances.iter_mut().find(|row| row.id == id).unwrap();
            source.tool = "claude".to_string();
            source.agent_session_id = Some("source-durable-sid".to_string());
            Ok(())
        })
        .unwrap();
    let target = Storage::new_unwatched("target").unwrap();
    target
        .update(|instances, _groups| {
            instances.push(Instance::new("victim", "/tmp/profile-reject/"));
            Ok(())
        })
        .unwrap();
    view.storages.insert("target".to_string(), target);
    view.selected_session = Some(id.clone());

    let result = view.restart_selected_session(
        Some("target"),
        Some("codex"),
        Some("--new"),
        Some("codex-wrapper"),
    );
    assert!(result.is_err());

    let source = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .unwrap();
    assert_eq!(source.tool, "claude");
    assert_eq!(
        source.agent_session_id.as_deref(),
        Some("source-durable-sid")
    );
    assert!(source.prior_tool_session_ids.is_empty());
    let live = view.get_instance(&id).unwrap();
    assert_eq!(live.source_profile, "test");
    assert_eq!(live.tool, "claude");
    assert_eq!(live.agent_session_id.as_deref(), Some("source-durable-sid"));
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

/// Snoozed siblings of the archive case: `snoozed_until` is also cleared
/// by `touch_last_accessed` and is also excluded from `merge_from_tui`,
/// so the same persistence bug applied to snoozed rows. Same fix path
/// (apply_user_action), same disk-versus-memory contract.
#[test]
#[serial]
fn stamp_last_accessed_on_snoozed_row_persistently_clears_snooze() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");

    view.apply_user_action(&id, |inst| inst.snooze(30))
        .expect("seed snooze must persist");
    assert!(
        view.get_instance(&id).unwrap().is_snoozed(),
        "precondition: row snoozed in memory"
    );

    view.stamp_last_accessed(&id);

    assert!(
        !view.get_instance(&id).unwrap().is_snoozed(),
        "stamp_last_accessed must clear snoozed_until in memory"
    );
    let disk_row = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|i| i.id == id)
        .expect("disk row present");
    assert!(
        disk_row.snoozed_until.is_none(),
        "stamp_last_accessed must persist the auto-unsnooze (merge_from_tui drops snoozed_until)"
    );
}
