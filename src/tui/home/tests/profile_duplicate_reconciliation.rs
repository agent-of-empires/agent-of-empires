use super::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

/// One session id present in `alpha` and `beta`, plus an optional valid
/// move journal claiming alpha -> beta (target published, target wins).
fn boot_ambiguous_state(with_journal: bool) -> (TempDir, AppDirGuard, String) {
    let temp = TempDir::new().unwrap();
    let guard = setup_test_home(&temp);
    let alpha = Storage::new_unwatched("alpha").unwrap();
    let mut inst = Instance::new("moved", "/repo/moved");
    inst.group_path = "work".to_string();
    let id = inst.id.clone();
    alpha
        .update(|i, g| {
            i.push(inst.clone());
            g.push(Group::new("work", "work"));
            Ok(())
        })
        .unwrap();
    let beta = Storage::new_unwatched("beta").unwrap();
    beta.update(|i, _| {
        let mut copy = inst.clone();
        copy.source_profile = "beta".to_string();
        i.push(copy);
        Ok(())
    })
    .unwrap();
    if with_journal {
        crate::session::record_move_journal(&crate::session::MoveJournalEntry {
            version: crate::session::MOVE_JOURNAL_VERSION,
            ids: vec![id.clone()],
            source_profile: "alpha".to_string(),
            target_profile: "beta".to_string(),
            source_sessions_path: alpha.sessions_path().to_path_buf(),
            target_sessions_path: beta.sessions_path().to_path_buf(),
            group_move_source_path: "work".to_string(),
            group_move_target_path: "moved".to_string(),
            group_move_subtree: false,
            created_at_epoch_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or_default(),
            authority: crate::session::move_journal::MoveAuthority::Metadata {
                source: crate::session::move_journal::ProfileAuthority::capture(&alpha).unwrap(),
                target: crate::session::move_journal::ProfileAuthority::capture(&beta).unwrap(),
                rows: vec![crate::session::move_journal::RowDelta {
                    before: alpha
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap(),
                    after: beta
                        .load()
                        .unwrap()
                        .into_iter()
                        .find(|row| row.id == id)
                        .unwrap(),
                }],
            },
        })
        .unwrap();
    }
    (temp, guard, id)
}

#[test]
#[serial]
fn interrupted_move_stays_excluded_until_worker_projects_authoritative_winner() {
    let (_temp, _guard, id) = boot_ambiguous_state(true);

    let mut view = HomeView::new_for_test(
        None,
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    assert!(
        view.instances.get(&id).is_none(),
        "both copies start excluded"
    );
    assert_eq!(view.legacy_duplicate_reports.len(), 1);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while view.journal_recovery_inflight
        || view.journal_recovery_dirty
        || view.pending_journal_reload
    {
        view.apply_reconcile_results();
        assert!(
            std::time::Instant::now() < deadline,
            "journal worker did not settle"
        );
        std::thread::yield_now();
    }
    assert_eq!(view.instances.len(), 1, "exactly the winning row publishes");
    let row = view.instances.get(&id).expect("repaired row present");
    assert_eq!(row.source_profile, "beta", "target copy wins per journal");
    assert!(view.legacy_duplicate_reports.is_empty());
    assert!(
        Storage::new_unwatched("alpha")
            .unwrap()
            .load()
            .unwrap()
            .is_empty(),
        "losing source copy removed on disk"
    );
}

#[test]
#[serial]
fn legacy_duplicate_stays_excluded_and_is_surfaced() {
    let (_temp, _guard, id) = boot_ambiguous_state(false);

    let mut view = HomeView::new_for_test(
        None,
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();

    assert!(
        view.instances.get(&id).is_none(),
        "without journal evidence every copy stays excluded"
    );
    assert_eq!(view.legacy_duplicate_reports.len(), 1);
    let message = view.legacy_duplicate_reports[0].actionable_message();
    assert!(message.contains(&id) && message.contains("alpha") && message.contains("beta"));

    // The fail-closed state must be visible, not silent.
    let theme = crate::tui::styles::load_theme("empire");
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal
        .draw(|f| {
            let area = f.area();
            view.render(f, area, &theme, None, None, None);
        })
        .unwrap();
    let buf = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buf.area.height {
        for x in 0..buf.area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    assert!(
        out.contains("\u{26a0} 1 ambiguous"),
        "the list title must flag ambiguous sessions.\nFull buffer:\n{out}"
    );
}

#[test]
#[serial]
fn legacy_intent_conflict_is_visible_at_startup_and_not_reopened_on_unchanged_reload() {
    let (_temp, _guard, id) = boot_ambiguous_state(false);
    let alpha = Storage::open_unwatched("alpha").unwrap();
    let beta = Storage::open_unwatched("beta").unwrap();
    let before_alpha = std::fs::read(alpha.sessions_path()).unwrap();
    let before_beta = std::fs::read(beta.sessions_path()).unwrap();
    crate::session::record_move_journal(&crate::session::MoveJournalEntry {
        version: crate::session::MOVE_JOURNAL_VERSION,
        ids: vec![id.clone()],
        source_profile: "alpha".into(),
        target_profile: "beta".into(),
        source_sessions_path: alpha.sessions_path().to_path_buf(),
        target_sessions_path: beta.sessions_path().to_path_buf(),
        group_move_source_path: "work".into(),
        group_move_target_path: "moved".into(),
        group_move_subtree: false,
        created_at_epoch_ms: chrono::Utc::now().timestamp_millis() as u64,
        authority: crate::session::move_journal::MoveAuthority::LegacyConflict {
            legacy: serde_json::json!({"version": 1, "ids": [id]}),
            reason: "legacy v1 intent has no original incarnation authority".into(),
            scope: crate::session::move_journal::JournalScope {
                profiles: vec![
                    crate::session::move_journal::ProfileAuthority::capture(&alpha).unwrap(),
                    crate::session::move_journal::ProfileAuthority::capture(&beta).unwrap(),
                ],
                resources: Vec::new(),
            },
        },
    })
    .unwrap();
    let mut view = HomeView::new_for_test(
        None,
        AvailableTools::with_tools(&["claude"]),
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    assert_eq!(
        view.info_dialog.as_ref().map(|dialog| dialog.title()),
        Some("Worktree Recovery Conflict")
    );
    assert_eq!(view.move_intent_conflicts.len(), 1);
    assert!(view.instances.get(&id).is_none());
    assert_eq!(std::fs::read(alpha.sessions_path()).unwrap(), before_alpha);
    assert_eq!(std::fs::read(beta.sessions_path()).unwrap(), before_beta);
    view.info_dialog = None;
    view.reload_storage_only().unwrap();
    assert!(
        view.info_dialog.is_none(),
        "an unchanged conflict must not reopen a dismissed dialog"
    );
    assert_eq!(view.move_intent_conflicts.len(), 1);
    assert_eq!(crate::session::move_journal::scan().entries.len(), 1);
}

fn rowless_purge_cleanup_fixture(
    storage: &Storage,
    project: &std::path::Path,
    state: &str,
) -> (Instance, std::path::PathBuf, std::path::PathBuf) {
    use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
    use crate::session::move_journal::{self, EffectPhase, MoveAuthority, ResourceEffect};
    let mut owner = Instance::new("rowless purge", &project.to_string_lossy());
    owner.source_profile = storage.profile().to_string();
    storage
        .update(|rows, _| {
            rows.push(owner.clone());
            Ok(())
        })
        .unwrap();
    let database = crate::session::get_app_dir().unwrap().join("acp_events.db");
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TABLE acp_events (session_id TEXT);
         CREATE TABLE acp_attachments (session_id TEXT);",
        )
        .unwrap();
    for table in ["acp_events", "acp_attachments"] {
        connection
            .execute(&format!("INSERT INTO {table} VALUES (?1)"), [&owner.id])
            .unwrap();
        connection
            .execute(&format!("INSERT INTO {table} VALUES ('unrelated')"), [])
            .unwrap();
    }
    let request = DeletionRequest {
        session_id: owner.id.clone(),
        instance: owner.clone(),
        delete_worktree: false,
        delete_branch: false,
        delete_sandbox: false,
        force_delete: false,
        detach_hooks: true,
        keep_scratch: false,
    };
    let PurgeReservation::Reserved(purge) =
        PurgeTransaction::reserve(storage.clone(), request).unwrap()
    else {
        panic!("rowless purge must reserve its real owner");
    };
    let committed = purge
        .run_hooks()
        .begin_irreversible()
        .unwrap_or_else(|error| panic!("purge row retirement failed: {error:?}"));
    assert!(storage.load().unwrap().iter().all(|row| row.id != owner.id));
    drop(committed);
    let path = move_journal::scan()
        .entries
        .into_iter()
        .find(|(_, entry)| {
            entry
                .as_ref()
                .is_ok_and(|entry| entry.ids.contains(&owner.id))
        })
        .unwrap()
        .0;
    let (journal, entry) = move_journal::try_recovery_lease(&path).unwrap().unwrap();
    let MoveAuthority::Resource { intent } = &entry.authority else {
        unreachable!()
    };
    let index = intent
        .effects
        .iter()
        .position(|effect| matches!(effect.effect, ResourceEffect::PurgeAcpEvents { .. }))
        .unwrap();
    let transaction =
        crate::session::claim::ResourceReservation::from_recovery(storage, journal, entry).unwrap();
    match state {
        "observed-error" => {
            connection.execute_batch("BEGIN IMMEDIATE").unwrap();
            let effect = transaction.effect(index).unwrap();
            let failed = transaction
                .run_effect(index, || {
                    crate::session::deletion::execute_purge_effect(&owner, &effect, false, |_| {
                        panic!("plain purge has no container")
                    })
                })
                .unwrap();
            assert!(
                failed.is_err(),
                "real SQLite lock must retain mandatory cleanup error"
            );
            connection.execute_batch("ROLLBACK").unwrap();
        }
        "pending" | "conflict" => {
            let root = crate::session::storage::acquire_ownership_lock().unwrap();
            transaction
                .begin_effect_with_ownership(&root, index)
                .unwrap();
            if state == "conflict" {
                let mut entry = transaction.journal().read().unwrap();
                let MoveAuthority::Resource { intent } = &mut entry.authority else {
                    unreachable!()
                };
                intent.phase = EffectPhase::Conflict;
                intent.effects[index].phase = EffectPhase::Conflict;
                intent.conflict = Some("effect completion could not be observed".into());
                transaction.journal().rewrite(&entry).unwrap();
            }
        }
        "prepared" => {}
        _ => unreachable!(),
    }
    drop(transaction);
    (owner, database, path)
}

#[test]
#[serial]
fn journal_recovery_runs_real_rowless_cleanup_off_input_and_coalesces_refresh() {
    use crate::session::move_journal::{EffectPhase, MoveAuthority, ResourceEffect};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    for state in ["prepared", "observed-error"] {
        for dispatch in ["reload", "disconnect"] {
            let native = TempDir::new().unwrap();
            let _tmux = crate::session::test_support::EnvGuard::set(&[(
                "AOE_TMUX_SOCKET",
                native.path().join("tmux.sock"),
            )]);
            let mut env = create_test_env_empty();
            let storage = env.view.storages["test"].clone();
            let project = env._temp.path().join("purged-project");
            std::fs::create_dir(&project).unwrap();
            let (owner, database, journal) =
                rowless_purge_cleanup_fixture(&storage, &project, state);
            let mut peer = Instance::new("peer before recovery", "/unrelated-peer");
            peer.source_profile = "test".into();
            let peer_id = peer.id.clone();
            storage
                .update(|rows, _| {
                    rows.push(peer);
                    Ok(())
                })
                .unwrap();
            let (ready_tx, ready_rx) = mpsc::channel();
            let (effect_tx, effect_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            let (released_tx, released_rx) = mpsc::channel();
            let held_database = database.clone();
            let holder = std::thread::spawn(move || {
                let connection = rusqlite::Connection::open(held_database).unwrap();
                connection.execute_batch("BEGIN EXCLUSIVE").unwrap();
                ready_tx.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let entry: crate::session::move_journal::MoveJournalEntry =
                        serde_json::from_slice(&std::fs::read(&journal).unwrap()).unwrap();
                    let MoveAuthority::Resource { intent } = entry.authority else {
                        unreachable!()
                    };
                    if intent.effects.iter().any(|effect| {
                        matches!(effect.effect, ResourceEffect::PurgeAcpEvents { .. })
                            && effect.phase == EffectPhase::Pending
                    }) {
                        break;
                    }
                    assert!(
                        Instant::now() < deadline,
                        "recovery did not begin its real effect"
                    );
                    std::thread::yield_now();
                }
                let root = loop {
                    if let Some(root) =
                        crate::session::storage::try_acquire_ownership_lock().unwrap()
                    {
                        break root;
                    }
                    assert!(Instant::now() < deadline, "physical recovery retained Root");
                    std::thread::yield_now();
                };
                drop(root);
                effect_tx.send(()).unwrap();
                let released = release_rx.recv_timeout(Duration::from_secs(3)).is_ok();
                connection.execute_batch("ROLLBACK").unwrap();
                released_tx.send(released).unwrap();
            });
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            if dispatch == "disconnect" {
                env.view.creation_poller.finish_for_test().unwrap();
                let mut data = creation_data(&project, "disconnected creation", "");
                data.profile = "test".into();
                env.view.create_session_with_hooks(data, None);
                assert_eq!(env.view.apply_creation_results(), None);
            } else {
                env.view.reload_storage_only().unwrap();
            }
            effect_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(env.view.journal_recovery_inflight);
            env.view.info_dialog = None;
            env.view.handle_key(key(KeyCode::Char('?')), None);
            let rendered_help = env.view.show_help
                && render_home_to_string(&mut env.view, 120, 35).contains("Keyboard Shortcuts");
            env.view.reload_storage_only().unwrap();
            let coalesced = env.view.journal_recovery_inflight && env.view.journal_recovery_dirty;
            storage
                .update(|rows, _| {
                    rows.iter_mut().find(|row| row.id == peer_id).unwrap().title =
                        "peer after recovery".into();
                    Ok(())
                })
                .unwrap();
            let _ = release_tx.send(());
            let returned_while_held = released_rx.recv().unwrap();
            holder.join().unwrap();
            assert!(
                returned_while_held && rendered_help,
                "{state}/{dispatch}: input or draw waited for cleanup"
            );
            assert!(
                coalesced,
                "{state}/{dispatch}: reload must coalesce behind owned job"
            );
            env.view.show_help = false;
            let deadline = Instant::now() + Duration::from_secs(10);
            while env.view.journal_recovery_inflight
                || env.view.journal_recovery_dirty
                || env.view.pending_journal_reload
            {
                env.view.apply_reconcile_results();
                assert!(Instant::now() < deadline, "recovery did not settle");
                std::thread::yield_now();
            }
            env.view.apply_creation_results();
            assert!(env.view.get_instance(&owner.id).is_none());
            assert_eq!(
                env.view.get_instance(&peer_id).unwrap().title,
                "peer after recovery"
            );
            assert_eq!(
                storage.load().unwrap().len(),
                1,
                "unrelated peer row survives"
            );
            let connection = rusqlite::Connection::open(&database).unwrap();
            for table in ["acp_events", "acp_attachments"] {
                let target: i64 = connection
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
                        [&owner.id],
                        |row| row.get(0),
                    )
                    .unwrap();
                let unrelated: i64 = connection
                    .query_row(
                        &format!("SELECT COUNT(*) FROM {table} WHERE session_id = 'unrelated'"),
                        [],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(target, 0, "{state}/{dispatch}/{table}");
                assert_eq!(unrelated, 1, "{state}/{dispatch}/{table}");
            }
            assert!(crate::session::move_journal::scan().entries.is_empty());
            env.view.apply_reconcile_results();
            assert!(
                !env.view.journal_recovery_inflight && !env.view.journal_recovery_dirty,
                "authoritative projection must not enqueue its own recovery"
            );
        }
    }
}

#[test]
#[serial]
fn journal_recovery_preserves_pending_and_conflicted_physical_cleanup() {
    for state in ["pending", "conflict"] {
        let native = TempDir::new().unwrap();
        let _tmux = crate::session::test_support::EnvGuard::set(&[(
            "AOE_TMUX_SOCKET",
            native.path().join("tmux.sock"),
        )]);
        let mut env = create_test_env_empty();
        let storage = env.view.storages["test"].clone();
        let project = env._temp.path().join("uncertain-project");
        std::fs::create_dir(&project).unwrap();
        let (owner, database, journal) = rowless_purge_cleanup_fixture(&storage, &project, state);
        env.view.reconcile_poller.set_healing_result_for_test(None);
        env.view.startup_recovery_gate = Some(std::time::Instant::now());
        env.view.reload_storage_only().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while env.view.journal_recovery_inflight || env.view.pending_journal_reload {
            env.view.apply_reconcile_results();
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(
            env.view.startup_recovery_gate.is_some(),
            "journal completion must not stand in for the separate healing result"
        );
        assert!(
            journal.exists(),
            "{state}: uncertainty must retain its journal"
        );
        let connection = rusqlite::Connection::open(database).unwrap();
        for table in ["acp_events", "acp_attachments"] {
            let target: i64 = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE session_id = ?1"),
                    [&owner.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(target, 1, "{state}: unobserved cleanup must not replay");
        }
        assert!(
            env.view.info_dialog.is_some(),
            "recovery conflict must be visible"
        );
    }
}

#[test]
#[serial]
fn unified_profile_discovery_defers_under_peer_root_and_retries_without_losing_refresh() {
    use std::sync::mpsc;
    use std::time::Duration;
    let (temp, guard) = test_home();
    seed_profile("alpha", &[Instance::new("existing", "/existing")]);
    let mut env = TestEnv {
        view: test_view(None),
        _guard: guard,
        _temp: temp,
    };
    let late = Storage::new_unwatched("late").unwrap();
    let peer = Instance::new("newly discovered", "/newly-discovered");
    let peer_id = peer.id.clone();
    late.update(|rows, _| {
        rows.push(peer);
        Ok(())
    })
    .unwrap();
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
    env.view.reload_storage_only().unwrap();
    let deferred = env.view.pending_storage_discovery
        && !env.view.storages.contains_key("late")
        && env.view.instances.len() == 1;
    env.view.handle_key(key(KeyCode::Char('?')), None);
    let rendered_help = env.view.show_help
        && render_home_to_string(&mut env.view, 120, 35).contains("Keyboard Shortcuts");
    let _ = release_tx.send(());
    let returned_while_held = released_rx.recv().unwrap();
    holder.join().unwrap();
    assert!(returned_while_held && deferred && rendered_help);
    env.view.apply_reconcile_results();
    assert!(!env.view.pending_storage_discovery);
    assert_eq!(
        env.view.get_instance(&peer_id).unwrap().source_profile,
        "late"
    );
}
