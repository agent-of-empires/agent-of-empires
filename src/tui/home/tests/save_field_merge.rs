use super::*;
use crate::session::Status;
use chrono::Utc;

mod metadata_abort_consumer {
    use super::*;
    use crate::session::{retained_intents, WorktreePathClaims};
    use crate::tui::dialogs::ContextMenuAction;
    use ratatui::layout::Rect;
    use serde_json::value::RawValue;
    use std::path::PathBuf;

    struct Fixture {
        env: TestEnv,
        storage: Storage,
        selected: String,
        peer: String,
        retired: String,
        ledger: PathBuf,
        protected: Vec<(PathBuf, Vec<u8>)>,
    }

    fn fixture(operation: LifecycleOperation, unknown: bool, creating: bool) -> Fixture {
        let (temp, guard) = test_home();
        let app = crate::session::get_app_dir().unwrap();
        retained_intents::initialize_legacy_in(&app).unwrap();
        let mut protected = Vec::new();
        for (directory, bytes) in [
            ("project", b"PROJECT CONTENT".as_slice()),
            ("claim", b"ORIGINAL CLAIM CONTENT".as_slice()),
            ("peer", b"PEER CONTENT".as_slice()),
            ("retired", b"PREVIOUS RETAINED CONTENT".as_slice()),
        ] {
            let directory = temp.path().join(directory);
            std::fs::create_dir(&directory).unwrap();
            let path = directory.join("protected");
            std::fs::write(&path, bytes).unwrap();
            protected.push((path, bytes.to_vec()));
        }
        let mut selected = Instance::new(
            "retained failed intent",
            temp.path().join("project").to_str().unwrap(),
        );
        selected.status = if creating {
            Status::Creating
        } else {
            Status::Stopped
        };
        selected.last_error = Some("original producer failed before publication".into());
        selected.lifecycle_generation = 1;
        let paths = vec![temp.path().join("claim")];
        selected.lifecycle_reservation = Some(LifecycleReservation {
            op: operation,
            generation: selected.lifecycle_generation,
            at: selected.created_at,
            path_claims: if unknown {
                WorktreePathClaims::Unknown(Some(paths))
            } else {
                WorktreePathClaims::Pending(paths)
            },
            custodian: None,
        });
        let mut peer = Instance::new("peer", temp.path().join("peer").to_str().unwrap());
        peer.status = Status::Stopped;
        let mut retired = selected.clone();
        retired.id = uuid::Uuid::new_v4().to_string();
        retired.title = "previous retained intent".into();
        retired.project_path = temp.path().join("retired").to_string_lossy().into_owned();
        retired.lifecycle_reservation.as_mut().unwrap().path_claims =
            WorktreePathClaims::Pending(vec![temp.path().join("retired")]);
        let storage = Storage::new_unwatched("test").unwrap();
        std::fs::write(
            storage.sessions_path(),
            serde_json::to_vec(&[&selected, &peer, &retired]).unwrap(),
        )
        .unwrap();
        let prior = retained_intents::capture(&storage, &retired.id).unwrap();
        retained_intents::abort(&prior).unwrap();
        let mut view = test_view(Some("test"));
        view.group_by = crate::session::config::GroupByMode::Manual;
        view.flat_items = view.build_flat_items();
        view.update_selected();
        Fixture {
            env: TestEnv {
                view,
                native_input: None,
                _guard: guard,
                _temp: temp,
            },
            storage,
            selected: selected.id,
            peer: peer.id,
            retired: retired.id,
            ledger: app.join("retained-intents.json"),
            protected,
        }
    }

    fn raw_rows(storage: &Storage) -> Vec<Box<RawValue>> {
        serde_json::from_slice(&std::fs::read(storage.sessions_path()).unwrap()).unwrap()
    }

    fn raw_owner(storage: &Storage, id: &str) -> String {
        raw_rows(storage)
            .into_iter()
            .find(|raw| serde_json::from_str::<serde_json::Value>(raw.get()).unwrap()["id"] == id)
            .unwrap()
            .get()
            .to_owned()
    }

    fn assert_protected(fixture: &Fixture) {
        for (path, before) in &fixture.protected {
            assert_eq!(&std::fs::read(path).unwrap(), before, "{}", path.display());
        }
    }

    fn open_abort_menu(fixture: &mut Fixture) {
        let view = &mut fixture.env.view;
        view.info_dialog = None;
        view.list_inner_area = Rect::new(1, 1, 28, 10);
        view.list_area = Rect::new(0, 0, 30, 12);
        let index = view
            .flat_items
            .iter()
            .position(|item| matches!(item, Item::Session { id, .. } if id == &fixture.selected))
            .unwrap();
        assert!(view.handle_right_click(5, 1 + index as u16));
        assert_eq!(
            view.selected_session.as_deref(),
            Some(fixture.selected.as_str())
        );
        let menu = view.context_menu.as_ref().unwrap();
        let abort_index = menu
            .items_for_test()
            .iter()
            .position(|(action, _)| *action == ContextMenuAction::AbortIntent)
            .expect("unfinished Create and Attach intents must expose metadata abort");
        for _ in 0..abort_index {
            assert!(view.handle_key(key(KeyCode::Down), None).is_none());
        }
        assert_eq!(
            view.context_menu.as_ref().unwrap().selected_action(),
            ContextMenuAction::AbortIntent
        );
        assert!(view.handle_key(key(KeyCode::Enter), None).is_none());
        assert!(view.context_menu.is_none());
    }

    fn confirm_abort(fixture: &mut Fixture) {
        assert!(fixture.env.view.confirm_dialog.is_some());
        assert_eq!(
            fixture
                .env
                .view
                .pending_claim_abort_confirmation
                .as_ref()
                .unwrap()
                .id(),
            fixture.selected
        );
        assert!(fixture.env.view.pending_creation_confirmation.is_none());
    }

    fn assert_aborted(fixture: &Fixture, original: &str, peer: &str, prior: &str) {
        let view = &fixture.env.view;
        assert!(view.get_instance(&fixture.selected).is_none());
        assert!(view.get_instance(&fixture.peer).is_some());
        assert_eq!(raw_owner(&fixture.storage, &fixture.peer), peer);
        assert_eq!(raw_rows(&fixture.storage).len(), 1);
        let ledger: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&fixture.ledger).unwrap()).unwrap();
        let records = ledger["records"].as_array().unwrap();
        let original_value: serde_json::Value = serde_json::from_str(original).unwrap();
        let prior: serde_json::Value = serde_json::from_str(prior).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], prior);
        assert_eq!(records[1]["source_profile"], "test");
        assert_eq!(records[1]["owner"], original_value);
        let owners =
            retained_intents::retained_raw_owners_in(&crate::session::get_app_dir().unwrap())
                .unwrap();
        assert_eq!(owners[1].get(), original);
        let retained =
            retained_intents::retained_ids_in(&crate::session::get_app_dir().unwrap()).unwrap();
        assert_eq!(retained.len(), 2);
        assert!(retained.contains(&fixture.selected));
        assert!(retained.contains(&fixture.retired));
        assert!(retained_intents::ensure_id_available(&fixture.selected).is_err());
        assert!(view.confirm_dialog.is_none());
        assert!(view.pending_claim_abort_confirmation.is_none());
        assert!(view.pending_creation_confirmation.is_none());
        assert!(view.persistence.created.is_empty());
        let message = view.info_dialog.as_ref().unwrap().message();
        assert!(message.contains("resources and permanent exclusions retained"));
        assert!(message.contains("No native cleanup performed"));
        assert!(!message.contains("Purged"));
        assert!(!message.contains("withdrawn"));
        assert_protected(fixture);
    }

    #[test]
    #[serial]
    fn creating_cancel_and_failed_recovery_can_abort_metadata_without_native_custody() {
        // These are durable lost-custody rows, not reconstructed native creation receipts.
        for cancelled in [false, true] {
            let mut fixture = fixture(LifecycleOperation::Create, cancelled, true);
            let original = raw_owner(&fixture.storage, &fixture.selected);
            let peer = raw_owner(&fixture.storage, &fixture.peer);
            let ledger_before = std::fs::read(&fixture.ledger).unwrap();
            let prior = serde_json::from_slice::<serde_json::Value>(&ledger_before).unwrap()
                ["records"][0]
                .to_string();
            if cancelled {
                let cancel = tokio_util::sync::CancellationToken::new();
                fixture.env.view.creating_stub_id = Some(fixture.selected.clone());
                fixture.env.view.creation_cancel = Some(cancel.clone());
                fixture.env.view.cancel_creation();
                assert!(cancel.is_cancelled());
                assert!(fixture.env.view.creating_stub_id.is_none());
                assert!(fixture.env.view.get_instance(&fixture.selected).is_some());
                assert_eq!(raw_owner(&fixture.storage, &fixture.selected), original);
            }
            for action in [
                ContextMenuAction::UndoCreation,
                ContextMenuAction::RetryCreationPublication,
            ] {
                fixture.env.view.selected_session = Some(fixture.selected.clone());
                fixture.env.view.dispatch_context_menu_action(action);
                assert!(drain_persistence(&mut fixture.env.view).is_err());
                assert!(fixture.env.view.pending_creation_confirmation.is_none());
                assert!(fixture.env.view.confirm_dialog.is_none());
                assert!(fixture
                    .env
                    .view
                    .info_dialog
                    .as_ref()
                    .unwrap()
                    .message()
                    .contains("actual original custodian"));
                assert_eq!(raw_owner(&fixture.storage, &fixture.selected), original);
                assert_eq!(std::fs::read(&fixture.ledger).unwrap(), ledger_before);
                assert_protected(&fixture);
            }
            open_abort_menu(&mut fixture);
            drain_persistence(&mut fixture.env.view).unwrap();
            confirm_abort(&mut fixture);
            assert!(fixture
                .env
                .view
                .handle_key(key(KeyCode::Esc), None)
                .is_none());
            drain_persistence(&mut fixture.env.view).unwrap();
            assert!(fixture.env.view.confirm_dialog.is_none());
            assert!(fixture.env.view.pending_claim_abort_confirmation.is_none());
            assert_eq!(raw_owner(&fixture.storage, &fixture.selected), original);
            assert_eq!(std::fs::read(&fixture.ledger).unwrap(), ledger_before);
            open_abort_menu(&mut fixture);
            drain_persistence(&mut fixture.env.view).unwrap();
            fixture.env.view.selected_session = Some(fixture.peer.clone());
            assert!(fixture
                .env
                .view
                .handle_key(key(KeyCode::Char('y')), None)
                .is_none());
            drain_persistence(&mut fixture.env.view).unwrap();
            assert_aborted(&fixture, &original, &peer, &prior);
        }
    }

    #[test]
    #[serial]
    fn queued_abort_prepare_refuses_a_changed_visible_intent_without_retaining_it() {
        let mut fixture = fixture(LifecycleOperation::Create, false, true);
        let ledger_before = std::fs::read(&fixture.ledger).unwrap();
        let workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        open_abort_menu(&mut fixture);
        let mut rows = raw_rows(&fixture.storage);
        let slot = rows
            .iter()
            .position(|raw| {
                serde_json::from_str::<serde_json::Value>(raw.get()).unwrap()["id"]
                    == fixture.selected
            })
            .unwrap();
        let mut changed: serde_json::Value = serde_json::from_str(rows[slot].get()).unwrap();
        changed["lifecycle_generation"] = serde_json::json!(2);
        changed["lifecycle_reservation"]["generation"] = serde_json::json!(2);
        rows[slot] = RawValue::from_string(serde_json::to_string(&changed).unwrap()).unwrap();
        std::fs::write(
            fixture.storage.sessions_path(),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();
        let rows_before: Vec<String> = raw_rows(&fixture.storage)
            .iter()
            .map(|row| row.get().to_owned())
            .collect();
        drop(workspace);
        assert!(drain_persistence(&mut fixture.env.view).is_err());
        assert!(fixture.env.view.pending_claim_abort_confirmation.is_none());
        assert!(fixture.env.view.confirm_dialog.is_none());
        assert!(fixture.env.view.info_dialog.is_some());
        assert_eq!(
            raw_rows(&fixture.storage)
                .iter()
                .map(|row| row.get().to_owned())
                .collect::<Vec<_>>(),
            rows_before
        );
        assert_eq!(std::fs::read(&fixture.ledger).unwrap(), ledger_before);
        assert!(fixture.env.view.get_instance(&fixture.selected).is_some());
        assert_protected(&fixture);
    }

    #[test]
    #[serial]
    fn queued_abort_confirmation_refuses_a_changed_raw_owner_without_recapture() {
        let mut fixture = fixture(LifecycleOperation::Create, false, true);
        let ledger_before = std::fs::read(&fixture.ledger).unwrap();
        open_abort_menu(&mut fixture);
        drain_persistence(&mut fixture.env.view).unwrap();
        confirm_abort(&mut fixture);
        let workspace = crate::session::acquire_session_workspace_claim_lock().unwrap();
        assert!(fixture
            .env
            .view
            .handle_key(key(KeyCode::Char('y')), None)
            .is_none());
        let mut rows = raw_rows(&fixture.storage);
        let slot = rows
            .iter()
            .position(|raw| {
                serde_json::from_str::<serde_json::Value>(raw.get()).unwrap()["id"]
                    == fixture.selected
            })
            .unwrap();
        let mut changed: serde_json::Value = serde_json::from_str(rows[slot].get()).unwrap();
        changed["peer_after_confirmation"] = serde_json::json!({"opaque": "preserve"});
        rows[slot] = RawValue::from_string(serde_json::to_string(&changed).unwrap()).unwrap();
        std::fs::write(
            fixture.storage.sessions_path(),
            serde_json::to_vec(&rows).unwrap(),
        )
        .unwrap();
        let rows_before: Vec<String> = raw_rows(&fixture.storage)
            .iter()
            .map(|row| row.get().to_owned())
            .collect();
        drop(workspace);
        assert!(drain_persistence(&mut fixture.env.view).is_err());
        assert!(fixture.env.view.pending_claim_abort_confirmation.is_none());
        assert!(fixture.env.view.confirm_dialog.is_none());
        assert!(fixture.env.view.info_dialog.is_some());
        assert_eq!(
            raw_rows(&fixture.storage)
                .iter()
                .map(|row| row.get().to_owned())
                .collect::<Vec<_>>(),
            rows_before
        );
        assert_eq!(std::fs::read(&fixture.ledger).unwrap(), ledger_before);
        assert!(fixture.env.view.get_instance(&fixture.selected).is_some());
        assert_protected(&fixture);
    }

    #[test]
    #[serial]
    fn ordinary_create_and_attach_intent_abort_ack_removes_only_the_confirmed_owner() {
        for (operation, unknown, legacy_journal) in [
            (LifecycleOperation::Create, true, false),
            (LifecycleOperation::Attach, false, false),
            (LifecycleOperation::Attach, true, false),
            (LifecycleOperation::Create, true, true),
        ] {
            let mut fixture = fixture(operation, unknown, false);
            if legacy_journal {
                let mut rows = raw_rows(&fixture.storage);
                let slot = rows
                    .iter()
                    .position(|raw| {
                        serde_json::from_str::<serde_json::Value>(raw.get()).unwrap()["id"]
                            == fixture.selected
                    })
                    .unwrap();
                let mut selected: serde_json::Value =
                    serde_json::from_str(rows[slot].get()).unwrap();
                let journal = selected["runner_journal"].as_object_mut().unwrap();
                journal.remove("creations");
                journal.remove("create_coverage");
                rows[slot] =
                    RawValue::from_string(serde_json::to_string(&selected).unwrap()).unwrap();
                std::fs::write(
                    fixture.storage.sessions_path(),
                    serde_json::to_vec(&rows).unwrap(),
                )
                .unwrap();
                fixture
                    .env
                    .view
                    .request_reload(super::super::super::ReloadKind::Full);
                drain_persistence(&mut fixture.env.view).unwrap();
            }
            let original = raw_owner(&fixture.storage, &fixture.selected);
            let peer = raw_owner(&fixture.storage, &fixture.peer);
            let prior = serde_json::from_slice::<serde_json::Value>(
                &std::fs::read(&fixture.ledger).unwrap(),
            )
            .unwrap()["records"][0]
                .to_string();
            open_abort_menu(&mut fixture);
            drain_persistence(&mut fixture.env.view).unwrap();
            confirm_abort(&mut fixture);
            fixture.env.view.selected_session = Some(fixture.peer.clone());
            assert!(fixture
                .env
                .view
                .handle_key(key(KeyCode::Char('y')), None)
                .is_none());
            drain_persistence(&mut fixture.env.view).unwrap();
            assert_aborted(&fixture, &original, &peer, &prior);
        }
    }
}

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

    {
        view.request_save();
        drain_persistence(&mut view)
    }
    .expect("save must merge peer writes");

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

    super::remove_test_instance(&mut view, &id);
    {
        view.request_save();
        drain_persistence(&mut view)
    }
    .expect("save must propagate the delete");

    let reloaded = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(
        !reloaded.iter().any(|i| i.id == id),
        "tombstoned row must be removed from disk"
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

    {
        let submitted = view.apply_user_action(&id, |inst| inst.archive());
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
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

    {
        let submitted = view.apply_user_action(&id, |inst| inst.archive());
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
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

    {
        let submitted = view.apply_user_action(&id, |inst| inst.archive());
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
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

    {
        view.request_save();
        drain_persistence(&mut view)
    }
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
    super::remove_test_instance(&mut view, &ephemeral_id);

    {
        view.request_save();
        drain_persistence(&mut view)
    }
    .expect("save must persist TUI-added row");

    let disk = Storage::new_unwatched("test").unwrap().load().unwrap();
    assert!(disk.iter().any(|i| i.id == added_id));
    assert!(!disk.iter().any(|i| i.id == ephemeral_id));
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
    (|| -> anyhow::Result<()> {
        let row = view.capture_transaction_row(&id)?;
        let target = view
            .storages
            .get("target")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fixture target storage missing"))?;
        let submitted = view.request_transaction(
            super::super::persistence_transactions::TransactionRequest::Move {
                row,
                target,
                requested: Box::new(requested),
                account_swap: false,
            },
        );
        await_transaction_result(&mut view, submitted)
    })()
    .unwrap();
    {
        view.request_reload(super::super::ReloadKind::Full);
        drain_persistence(&mut view)
    }
    .unwrap();

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
    (|| -> anyhow::Result<()> {
        let row = view.capture_transaction_row(&id)?;
        let target = view
            .storages
            .get("test")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fixture target storage missing"))?;
        let submitted = view.request_transaction(
            super::super::persistence_transactions::TransactionRequest::Move {
                row,
                target,
                requested: Box::new(requested),
                account_swap: false,
            },
        );
        await_transaction_result(&mut view, submitted)
    })()
    .unwrap();
    assert_eq!(view.get_instance(&id).unwrap().group_path, "newgrp");

    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );
    let mut requested = view.get_instance(&id).unwrap().clone();
    requested.group_path.clear();
    (|| -> anyhow::Result<()> {
        let row = view.capture_transaction_row(&id)?;
        let target = view
            .storages
            .get("target")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fixture target storage missing"))?;
        let submitted = view.request_transaction(
            super::super::persistence_transactions::TransactionRequest::Move {
                row,
                target,
                requested: Box::new(requested),
                account_swap: false,
            },
        );
        await_transaction_result(&mut view, submitted)
    })()
    .unwrap();
    {
        view.request_save();
        drain_persistence(&mut view)
    }
    .expect("save must succeed across profiles");

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

    let error = {
        let submitted = view.restart_selected_session(Some("target"), Some("claude"), None, None);
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
    .expect_err("target identity collision must reject restart profile move");

    assert!(error
        .to_string()
        .contains("Session already exists with same title and path"));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
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
fn group_profile_move_rejects_a_changed_original_generation() {
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
    view.group_rename_context = Some(super::super::GroupRenameContext {
        old_path: "work".to_string(),
        old_profile: "alpha".to_string(),
    });

    {
        let submitted = view.rename_selected_group(None, Some("beta"));
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
    .expect_err("a changed original generation must abort the acknowledged move");

    assert!(!source.load().unwrap().is_empty());
    let unchanged = source
        .load()
        .unwrap()
        .into_iter()
        .find(|row| row.id == id)
        .expect("the original source row must be retained");
    assert_eq!(unchanged.title, "peer-title");
    assert_eq!(unchanged.lifecycle_generation, 7);
    assert!(target.load().unwrap().is_empty());
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

    {
        view.request_reload(super::super::ReloadKind::Full);
        drain_persistence(&mut view)
    }
    .unwrap();
    let authoritative = view.get_instance(&id).cloned().unwrap();
    assert_eq!(authoritative.title, "peer-new-title");
    assert_eq!(authoritative.lifecycle_generation, 7);
    assert_eq!(authoritative.status, Status::Running);

    let requested = authoritative.clone();
    (|| -> anyhow::Result<()> {
        let row = view.capture_transaction_row(&id)?;
        let target = view
            .storages
            .get("target")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fixture target storage missing"))?;
        let submitted = view.request_transaction(
            super::super::persistence_transactions::TransactionRequest::Move {
                row,
                target,
                requested: Box::new(requested),
                account_swap: false,
            },
        );
        await_transaction_result(&mut view, submitted)
    })()
    .unwrap();
    {
        view.request_save();
        drain_persistence(&mut view)
    }
    .unwrap();

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
        path_claims: crate::session::WorktreePathClaims::None,
        custodian: None,
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

    {
        view.request_reload(super::super::ReloadKind::Full);
        drain_persistence(&mut view)
    }
    .unwrap();
    let requested = view.get_instance(&id).cloned().unwrap();
    let error = (|| -> anyhow::Result<()> {
        let row = view.capture_transaction_row(&id)?;
        let target = view
            .storages
            .get("target")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fixture target storage missing"))?;
        let submitted = view.request_transaction(
            super::super::persistence_transactions::TransactionRequest::Move {
                row,
                target,
                requested: Box::new(requested),
                account_swap: false,
            },
        );
        await_transaction_result(&mut view, submitted)
    })()
    .expect_err("reserved session must not move profiles");

    assert!(error
        .to_string()
        .contains("lifecycle operation is in progress"));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
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
    (|| -> anyhow::Result<()> {
        let row = view.capture_transaction_row(&id)?;
        let target = view
            .storages
            .get("target")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("fixture target storage missing"))?;
        let submitted = view.request_transaction(
            super::super::persistence_transactions::TransactionRequest::Move {
                row,
                target,
                requested: Box::new(requested),
                account_swap: false,
            },
        );
        await_transaction_result(&mut view, submitted)
    })()
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

    let error = {
        let submitted =
            view.restart_selected_session(Some("missing-target"), Some("claude"), None, None);
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
    .expect_err("missing target profile must reject restart profile move");
    assert!(error
        .to_string()
        .contains("Profile 'missing-target' does not exist"));
    assert!(!crate::session::list_profiles()
        .unwrap()
        .contains(&"missing-target".to_string()));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");

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

    let error = {
        let submitted = view.restart_selected_session(Some("target"), Some("claude"), None, None);
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
    .expect_err("target identity collision must reject restart profile move");

    assert!(error
        .to_string()
        .contains("Session already exists with same title and path"));
    assert_eq!(view.get_instance(&id).unwrap().source_profile, "test");
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
fn test_reload_honors_peer_cleared_session_id() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/sid");

    // Seed a stale sid via the in-memory mirror + persist.
    view.mutate_instance(&id, |inst| {
        inst.agent_session_id = Some("stale_X".to_string());
    });
    {
        view.request_save();
        drain_persistence(&mut view)
    }
    .unwrap();

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

    {
        view.request_reload(super::super::ReloadKind::Full);
        drain_persistence(&mut view)
    }
    .unwrap();

    assert!(
        view.get_instance(&id)
            .and_then(|i| i.agent_session_id.clone())
            .is_none(),
        "reload must honor peer-cleared sid; carrying memory would re-pass --resume <stale>"
    );
}

/// `stamp_last_accessed` on a sunk row must auto-clear archived_at in memory and on disk
/// and rebuild flat_items, so the row leaves the Archived section on the same frame. The old
/// mutate_instance + save path left it stuck until `z`, because merge_from_tui doesn't carry
/// archived_at and the next reload resurrected the sink.
#[test]
#[serial]
fn stamp_last_accessed_on_archived_row_unsinks_persistently() {
    use crate::session::{is_archived_section_path, Item};

    let (_temp, _guard, mut view, id) = boot_view_with_one_session("session", "/tmp/grp");

    {
        let submitted = view.apply_user_action(&id, |inst| inst.archive());
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
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
    drain_persistence(&mut view).unwrap();

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

    // Snoozed sibling: `snoozed_until` is also excluded from `merge_from_tui`.
    {
        let submitted = view.apply_user_action(&id, |inst| inst.snooze(30));
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
    .expect("seed snooze must persist");
    assert!(view.get_instance(&id).unwrap().is_snoozed());
    view.stamp_last_accessed(&id);
    assert!(!view.get_instance(&id).unwrap().is_snoozed());
    let disk_row = Storage::new_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .into_iter()
        .find(|i| i.id == id)
        .expect("disk row present");
    assert!(
        disk_row.snoozed_until.is_none(),
        "stamp_last_accessed must persist the auto-unsnooze"
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

    {
        let submitted = view.restart_selected_session(
            Some("target"),
            Some("codex"),
            Some("--fast"),
            Some("codex-wrapper"),
        );
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
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

/// A restart that moves profiles AND swaps to another account of the same agent
/// must land the moved row with its conversation intact, from the locked source
/// row rather than the TUI snapshot. Parking it there would orphan the
/// transcript the carry copied into the incoming account (#4030).
#[test]
#[serial]
fn restart_profile_move_account_swap_keeps_the_conversation() {
    let (_temp, _guard, mut view, id) =
        boot_view_with_one_session("victim", "/tmp/profile-account");
    let app_dir = crate::session::get_app_dir().expect("app dir");
    std::fs::create_dir_all(&app_dir).expect("app dir");
    std::fs::write(
        app_dir.join("config.toml"),
        "[session.agent_detect_as]\n\
         claude-1 = \"claude\"\n\
         claude-2 = \"claude\"\n",
    )
    .expect("config");
    let _registry_test = crate::tmux::status_rules::ProfileRegistryGuard::take("test");
    let _registry_target = crate::tmux::status_rules::ProfileRegistryGuard::take("target");
    crate::session::config::profile_config::resolve_config_or_warn("test");
    crate::session::config::profile_config::resolve_config_or_warn("target");

    fn seed(instance: &mut Instance) {
        instance.tool = "claude-1".to_string();
        instance.detect_as = "claude".to_string();
        instance.agent_session_id = Some("snapshot-sid".to_string());
    }
    view.mutate_instance(&id, seed);
    view.storages["test"]
        .update(|instances, _groups| {
            seed(instances.iter_mut().find(|row| row.id == id).unwrap());
            Ok(())
        })
        .unwrap();
    // A poller lands a fresher conversation on disk than the TUI mirror holds.
    view.storages["test"]
        .update(|instances, _groups| {
            instances
                .iter_mut()
                .find(|row| row.id == id)
                .unwrap()
                .agent_session_id = Some("durable-sid".to_string());
            Ok(())
        })
        .unwrap();
    view.storages.insert(
        "target".to_string(),
        Storage::new_unwatched("target").unwrap(),
    );
    view.selected_session = Some(id.clone());

    {
        let submitted = view.restart_selected_session(Some("target"), Some("claude-2"), None, None);
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    }
    .unwrap();

    let target_rows = Storage::new_unwatched("target").unwrap().load().unwrap();
    let moved = target_rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(moved.tool, "claude-2");
    assert_eq!(
        moved.agent_session_id.as_deref(),
        Some("durable-sid"),
        "the moved row must resume the locked source row's conversation"
    );
    assert!(
        !moved.prior_tool_session_ids.contains_key("claude-1"),
        "an account swap carries the conversation rather than parking it"
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

    let result = {
        let submitted = view.restart_selected_session(
            Some("target"),
            Some("codex"),
            Some("--new"),
            Some("codex-wrapper"),
        );
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    };
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
    assert!(!view.restart_in_flight.contains_key(&id));
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

    let result = {
        let submitted =
            view.rename_selected("new-name", Some("renamed/group"), Some("target"), false);
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    };
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

    let result = {
        let submitted = view.rename_selected("new-name", None, Some("target"), true);
        await_transaction_result(
            &mut view,
            submitted.map(|_| super::super::TransactionDisposition::Queued),
        )
    };

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

#[test]
#[serial]
fn cached_save_cannot_adopt_recreated_profile_or_replay_pending_metadata() {
    let (temp, _guard, mut view, id) =
        boot_view_with_one_session("old-title", "/tmp/profile-origin");
    let original = view.storages["test"].clone();
    view.mutate_instance(&id, |row| row.status = Status::Running);
    let row_token = view.record_row_edit("test", &id);
    let group_token = view.record_group_edit("test");
    view.pending_deletions
        .entry("test".into())
        .or_default()
        .insert(
            id.clone(),
            super::super::persistence_worker::RowDeletion {
                revision: row_token,
                created_at: view.instances[&id].created_at,
            },
        );
    view.pending_group_deletions
        .entry("test".into())
        .or_default()
        .insert("shared".into(), group_token);
    let mut pending = Instance::new("old-pending", "/tmp/old-pending");
    pending.source_profile = "test".into();
    let pending_id = pending.id.clone();
    view.add_instance(pending);
    view.group_trees
        .get_mut("test")
        .unwrap()
        .create_group("old-pending-group");
    std::fs::rename(
        original.sessions_path().parent().unwrap(),
        temp.path().join("retired-profile"),
    )
    .unwrap();
    crate::session::create_profile("test").unwrap();
    let replacement = Storage::new_unwatched("test").unwrap();
    let mut row = Instance::new("replacement", "/tmp/profile-origin");
    row.id = id.clone();
    row.source_profile = "test".into();
    row.group_path = "shared".into();
    replacement
        .update(|rows, groups| {
            rows.push(row);
            groups.push(Group::new("shared", "replacement-group"));
            Ok(())
        })
        .unwrap();
    let groups_path = replacement
        .sessions_path()
        .parent()
        .unwrap()
        .join("groups.json");
    let rows_before = std::fs::read(replacement.sessions_path()).unwrap();
    let groups_before = std::fs::read(&groups_path).unwrap();
    assert!(
        {
            view.request_save();
            drain_persistence(&mut view)
        }
        .is_err(),
        "a cached writer must reject the new physical profile"
    );
    assert_eq!(
        std::fs::read(replacement.sessions_path()).unwrap(),
        rows_before
    );
    assert_eq!(std::fs::read(&groups_path).unwrap(), groups_before);
    {
        view.request_reload(super::super::ReloadKind::Full);
        drain_persistence(&mut view)
    }
    .unwrap();
    assert_eq!(view.get_instance(&id).unwrap().title, "replacement");
    assert_ne!(view.get_instance(&id).unwrap().status, Status::Running);
    assert!(view.get_instance(&pending_id).is_none());
    {
        view.request_save();
        drain_persistence(&mut view)
    }
    .unwrap();
    let rows = replacement.load().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "replacement");
    let groups: Vec<Group> = serde_json::from_slice(&std::fs::read(groups_path).unwrap()).unwrap();
    assert!(groups.iter().any(|group| group.path == "shared"));
    assert!(!groups.iter().any(|group| group.path == "old-pending-group"));
}

#[test]
#[serial]
fn archive_completion_uses_captured_id_after_selection_changes() {
    let (_temp, _guard, mut view, original_id) =
        boot_view_with_one_session("original", "/tmp/original-archive");
    let storage = view.storages["test"].clone();
    let peer = Instance::new("other", "/tmp/other-archive");
    let peer_id = peer.id.clone();
    storage
        .update(|rows, _| {
            rows.push(peer);
            Ok(())
        })
        .unwrap();
    {
        view.request_reload(super::super::ReloadKind::Full);
        drain_persistence(&mut view)
    }
    .unwrap();
    view.select_session_by_id(&original_id);
    let held = storage
        .acquire_instance_lifecycle_lock(&original_id)
        .unwrap();
    view.toggle_archive_at_cursor().unwrap();
    assert!(!view.persistence_is_idle());
    assert!(
        !view.get_instance(&original_id).unwrap().is_archived(),
        "queueing must not publish archive before settlement"
    );
    view.select_session_by_id(&peer_id);
    drop(held);
    drain_persistence(&mut view).unwrap();
    finish_runner_settlements(&mut view);
    let rows = storage.load().unwrap();
    let archived = rows.iter().find(|row| row.id == original_id).unwrap();
    assert!(archived.is_archived());
    assert!(archived.lifecycle_reservation.is_none());
    assert!(
        !rows
            .iter()
            .find(|row| row.id == peer_id)
            .unwrap()
            .is_archived(),
        "a changed selection must not retarget the queued action"
    );
}

#[test]
#[serial]
fn restart_failed_save_fence_never_starts_or_submits_native_work() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let before = env.view.get_instance(&id).unwrap().clone();
    env.view
        .storages
        .get_mut("test")
        .unwrap()
        .set_fail_writes_for_test(true);
    let accepted = env.view.restart_selected_session(
        None,
        Some("codex"),
        Some("--new-args"),
        Some("replacement"),
    );
    assert!(accepted.is_ok());
    assert!(!env.view.restart_in_flight.contains_key(&id));
    assert_eq!(env.view.get_instance(&id).unwrap().status, before.status);
    assert!(drain_persistence(&mut env.view).is_err());
    assert!(!env.view.restart_in_flight.contains_key(&id));
    assert_eq!(env.view.get_instance(&id).unwrap().tool, before.tool);
    assert_eq!(env.view.get_instance(&id).unwrap().command, before.command);
    assert_eq!(
        Storage::open_unwatched("test").unwrap().load().unwrap()[0].tool,
        before.tool
    );
}

#[test]
#[serial]
fn profile_switch_is_nonblocking_and_waits_for_old_profile_save_ack() {
    let (_temp, _guard, mut view, id) = boot_view_with_one_session("source", "/tmp/switch-fence");
    Storage::new_unwatched("target").unwrap();
    view.mutate_instance(&id, |row| row.command = "accepted-before-switch".into());
    let held = crate::session::acquire_session_workspace_claim_lock().unwrap();
    let start = std::time::Instant::now();
    let accepted = view.switch_profile(Some("target".into()));
    assert!(accepted.is_ok());
    assert!(start.elapsed() < std::time::Duration::from_millis(250));
    assert_eq!(view.active_profile.as_deref(), Some("test"));
    assert!(!view.persistence_is_idle());
    drop(held);
    drain_persistence(&mut view).unwrap();
    assert_eq!(view.active_profile.as_deref(), Some("target"));
    assert_eq!(
        Storage::open_unwatched("test").unwrap().load().unwrap()[0].command,
        "accepted-before-switch"
    );
}

#[test]
#[serial]
fn failed_old_profile_save_aborts_switch_and_preserves_active_profile() {
    let mut env = create_test_env_with_sessions(1);
    Storage::new_unwatched("target").unwrap();
    env.view
        .storages
        .get_mut("test")
        .unwrap()
        .set_fail_writes_for_test(true);
    env.view.switch_profile(Some("target".into())).unwrap();
    assert!(drain_persistence(&mut env.view).is_err());
    assert_eq!(env.view.active_profile.as_deref(), Some("test"));
}

#[test]
#[serial]
fn metadata_failure_rolls_back_only_its_shadow_and_emits_no_success_action() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view
        .storages
        .get_mut("test")
        .unwrap()
        .set_fail_writes_for_test(true);
    env.view.snooze_session_for(&id, 60).unwrap();
    assert!(env
        .view
        .take_persistence_action()
        .map(super::super::PersistenceAction::into_action)
        .is_none());
    assert!(drain_persistence(&mut env.view).is_err());
    assert!(!env.view.get_instance(&id).unwrap().is_snoozed());
    assert!(env
        .view
        .take_persistence_action()
        .map(super::super::PersistenceAction::into_action)
        .is_none());
    assert!(!Storage::open_unwatched("test").unwrap().load().unwrap()[0].is_snoozed());
}

#[test]
#[serial]
fn consecutive_pending_metadata_toggles_preserve_semantic_order() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.toggle_favorite_at_cursor().unwrap();
    env.view.toggle_favorite_at_cursor().unwrap();
    drain_persistence(&mut env.view).unwrap();
    assert!(!env.view.get_instance(&id).unwrap().is_favorited());
    assert!(!Storage::open_unwatched("test").unwrap().load().unwrap()[0].is_favorited());
}

#[test]
#[serial]
fn historical_creating_row_cannot_register_or_reconstruct_original_custody() {
    let mut row = Instance::new("historical", "/tmp/historical-create");
    row.status = Status::Creating;
    let mut env = seeded_env(test_home(), &[row], false);
    let id = env.view.instance_at(0).id.clone();
    env.view.selected_session = Some(id.clone());
    env.view.prompt_creation_recovery(
        super::super::persistence_transactions::CreationRecoveryAction::Undo,
    );
    assert!(drain_persistence(&mut env.view).is_err());
    assert!(env.view.pending_creation_confirmation.is_none());
    assert!(env
        .view
        .info_dialog
        .as_ref()
        .unwrap()
        .message()
        .contains("actual original custodian"));
    assert!(Storage::open_unwatched("test")
        .unwrap()
        .load()
        .unwrap()
        .iter()
        .any(|row| row.id == id));
}

#[test]
#[serial]
fn transaction_behind_failed_save_requires_its_own_successful_original_fence() {
    let mut env = create_test_env_with_sessions(1);
    let id = env.view.instance_at(0).id.clone();
    let before = env.view.get_instance(&id).unwrap().clone();
    env.view
        .storages
        .get_mut("test")
        .unwrap()
        .set_fail_writes_for_test(true);
    env.view.request_save();
    env.view
        .restart_selected_session(None, Some("codex"), None, None)
        .unwrap();
    assert!(drain_persistence(&mut env.view).is_err());
    assert!(!env.view.restart_in_flight.contains_key(&id));
    assert_eq!(env.view.get_instance(&id).unwrap().status, before.status);
    assert_eq!(
        Storage::open_unwatched("test").unwrap().load().unwrap()[0].tool,
        before.tool
    );
}
