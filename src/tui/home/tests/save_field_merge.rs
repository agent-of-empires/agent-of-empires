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
    let view = HomeView::new_for_test(
        Some("test".to_string()),
        tools,
        crate::file_watch::FileWatchService::noop(),
    )
    .unwrap();
    (temp, guard, view, id)
}

fn boot_view_with_dirty_worktree() -> (TempDir, AppDirGuard, HomeView, String, TempDir) {
    let (temp, home, mut view, id) = boot_view_with_one_session("retry", "/tmp/retry");
    let (repository, _) = crate::git::test_support::init_repo();
    let project = temp.path().join("retry-worktree");
    crate::git::test_support::run_git(
        repository.path(),
        &[
            "-c",
            "core.hooksPath=/dev/null",
            "worktree",
            "add",
            "-b",
            "retry-kept",
            project.to_str().unwrap(),
        ],
    );
    std::fs::write(project.join("protected"), b"dirty worktree").unwrap();
    let storage = Storage::open_unwatched("test").unwrap();
    storage
        .update(|rows, _| {
            rows[0].project_path = project.to_string_lossy().into_owned();
            rows[0].worktree_info = Some(crate::session::WorktreeInfo {
                branch: "retry-kept".into(),
                main_repo_path: repository.path().to_string_lossy().into_owned(),
                managed_by_aoe: true,
                created_at: Utc::now(),
                base_branch: None,
            });
            rows[0].trash();
            Ok(())
        })
        .unwrap();
    let config = serde_json::from_value(serde_json::json!({
        "worktree": { "auto_cleanup": true, "delete_branch_on_cleanup": false }
    }))
    .unwrap();
    crate::session::config::profile_config::save_profile_config("test", &config).unwrap();
    view.reload_storage_only().unwrap();
    (temp, home, view, id, repository)
}

#[test]
#[serial]
fn pending_purge_rejects_force_remove_and_applies_its_real_success_or_failure() {
    use super::super::DeleteWorker;
    use crate::session::deletion::{execute_deletion, DeletionDisposition};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let _git_env = crate::session::test_support::EnvGuard::set(&[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]);
    for forced in [false, true] {
        let (_temp, _home, mut view, id, repository) = boot_view_with_dirty_worktree();
        let storage = Storage::open_unwatched("test").unwrap();
        let before = storage.load().unwrap().remove(0);
        let before_bytes = std::fs::read(storage.sessions_path()).unwrap();
        let project = std::path::PathBuf::from(&before.project_path);
        view.trashed_section_collapsed = false;
        view.rebuild_flat_items();
        view.cursor = view
            .flat_items
            .iter()
            .position(
                |item| matches!(item, Item::Session { id: candidate, .. } if candidate == &id),
            )
            .unwrap();
        view.update_selected();
        view.view_mode = ViewMode::Structured;
        view.strict_hotkeys = false;
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        view.deletion_poller =
            crate::tui::deletion_poller::DeletionPoller::with_handler_for_test(move |request| {
                assert!(request.delete_worktree);
                assert_eq!(request.force_delete, forced);
                entered_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                let result = execute_deletion(request);
                assert_eq!(
                    result.disposition,
                    if forced {
                        DeletionDisposition::Removed
                    } else {
                        DeletionDisposition::Failed
                    }
                );
                finished_tx.send(result.errors.join("; ")).unwrap();
                result
            });
        view.handle_key(key(KeyCode::Char('d')), None);
        assert!(view.unified_delete_dialog.is_some());
        if forced {
            view.handle_key(key(KeyCode::Tab), None);
            view.handle_key(key(KeyCode::Char(' ')), None);
        }
        view.handle_key(key(KeyCode::Char('y')), None);
        assert!(view.unified_delete_dialog.is_none());
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let attempt = view.deletes_in_flight[&id].clone();
        let counter = view.next_delete_attempt_id;
        assert!(attempt.worker == DeleteWorker::Purge);
        assert_eq!(view.instances[&id].status, Status::Deleting);
        assert_eq!(
            std::fs::read(storage.sessions_path()).unwrap(),
            before_bytes
        );
        assert!(before.lifecycle_reservation.is_none());

        view.handle_key(key(KeyCode::Char('d')), None);
        assert_eq!(
            view.info_dialog.as_ref().map(InfoDialog::title),
            Some("Deletion pending")
        );
        assert!(view.confirm_dialog.is_none());
        assert!(view.pending_force_remove_session.is_none());
        view.handle_key(key(KeyCode::Char('y')), None);
        if view.info_dialog.is_some() {
            view.handle_key(key(KeyCode::Esc), None);
        }
        assert!(view.info_dialog.is_none());
        let error = view.force_remove_session(&id).unwrap_err();
        assert!(error.to_string().contains("pending deletion"));
        let current = &view.deletes_in_flight[&id];
        assert!(current.worker == DeleteWorker::Purge);
        assert_eq!(current.attempt_id, attempt.attempt_id);
        assert_eq!(current.source_profile, attempt.source_profile);
        assert_eq!(current.lifecycle_generation, attempt.lifecycle_generation);
        assert_eq!(current.forced, attempt.forced);
        assert_eq!(current.trashed_at, attempt.trashed_at);
        assert_eq!(current.trash_lifecycle_id, attempt.trash_lifecycle_id);
        assert_eq!(view.next_delete_attempt_id, counter);
        assert!(!view.drop_poller.is_pending(&id));
        assert!(view.failed_deletes.is_empty());
        assert_eq!(
            std::fs::read(storage.sessions_path()).unwrap(),
            before_bytes
        );

        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while view.deletes_in_flight.contains_key(&id) {
            view.apply_deletion_results();
            assert!(
                Instant::now() < deadline,
                "original purge completion was not applied"
            );
            std::thread::yield_now();
        }
        let error = finished_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            view.info_dialog.is_none(),
            "no refused Drop should replace the original result"
        );
        if forced {
            assert!(error.is_empty());
            assert!(!view.instances.contains_key(&id));
            assert!(storage.load().unwrap().is_empty());
            assert!(!project.exists());
        } else {
            assert!(!error.is_empty());
            let durable = storage.load().unwrap().remove(0);
            let cached = &view.instances[&id];
            assert_eq!(cached.status, Status::Error);
            assert_eq!(cached.last_error.as_deref(), Some(error.as_str()));
            assert_eq!(cached.lifecycle_generation, before.lifecycle_generation + 2);
            assert_eq!(cached.lifecycle_generation, durable.lifecycle_generation);
            assert!(durable.lifecycle_reservation.is_none());
            assert_eq!(view.failed_delete_forced(cached), Some(false));
            assert_eq!(
                std::fs::read(project.join("protected")).unwrap(),
                b"dirty worktree"
            );
        }
        assert!(git2::Repository::open(repository.path())
            .unwrap()
            .find_branch("retry-kept", git2::BranchType::Local)
            .is_ok());
    }
}

#[test]
#[serial]
fn failed_forced_trash_purge_can_still_drop_its_row_without_worktree_cleanup() {
    use super::super::DeleteWorker;
    use crate::session::deletion::execute_deletion;
    use crate::session::lifecycle_journal::{filename_key, read_deletion, LifecyclePhase};
    use crate::tui::dialogs::DeleteOptions;
    use std::time::{Duration, Instant};
    let _git_env = crate::session::test_support::EnvGuard::set(&[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]);
    let (_temp, _home, mut view, id, repository) = boot_view_with_dirty_worktree();
    let storage = Storage::open_unwatched("test").unwrap();
    let before = storage.load().unwrap().remove(0);
    let project = std::path::PathBuf::from(&before.project_path);
    let failed_id = id.clone();
    view.deletion_poller =
        crate::tui::deletion_poller::DeletionPoller::with_handler_for_test(move |request| {
            let failed_id = failed_id.clone();
            let _sync = crate::session::intercept_directory_sync_for_test(move |path| {
                if filename_key(path).is_ok_and(|key| key.session_id() == failed_id)
                    && read_deletion(path)?
                        .is_some_and(|entry| entry.phase == LifecyclePhase::HooksStarted)
                {
                    anyhow::bail!(std::io::Error::from_raw_os_error(libc::EIO));
                }
                Ok(())
            });
            execute_deletion(request)
        });
    view.selected_session = Some(id.clone());
    view.delete_selected(&DeleteOptions {
        delete_worktree: true,
        force_delete: true,
        ..Default::default()
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while view.deletes_in_flight.contains_key(&id) {
        view.apply_deletion_results();
        assert!(
            Instant::now() < deadline,
            "forced preparation failure did not arrive"
        );
        std::thread::yield_now();
    }
    assert_eq!(view.instances[&id].status, Status::Error);
    assert!(view.instances[&id]
        .last_error
        .as_deref()
        .unwrap()
        .contains("hook start"));
    assert_eq!(
        view.instances[&id].lifecycle_generation,
        before.lifecycle_generation + 2
    );
    assert_eq!(view.failed_delete_forced(&view.instances[&id]), Some(true));
    let durable = storage.load().unwrap().remove(0);
    assert!(durable.same_trash_lifecycle(&before));
    assert!(durable.lifecycle_reservation.is_none());
    view.empty_trash_all(false, true);
    assert!(view.deletes_in_flight[&id].worker == DeleteWorker::Drop);
    let deadline = Instant::now() + Duration::from_secs(5);
    while view.deletes_in_flight.contains_key(&id) {
        view.apply_drop_results();
        assert!(
            Instant::now() < deadline,
            "failed-forced-trash Drop did not complete"
        );
        std::thread::yield_now();
    }
    assert!(!view.instances.contains_key(&id));
    assert!(!view.failed_deletes.contains_key(&id));
    assert!(storage.load().unwrap().is_empty());
    crate::session::deletion::recover_lifecycle_journals_once().unwrap();
    assert_eq!(
        std::fs::read(project.join("protected")).unwrap(),
        b"dirty worktree"
    );
    assert!(git2::Repository::open(repository.path())
        .unwrap()
        .find_branch("retry-kept", git2::BranchType::Local)
        .is_ok());
}

#[test]
#[serial]
fn dirty_purge_rollback_retires_the_request_after_reload_and_preserves_a_profile_move() {
    use crate::session::deletion::{execute_deletion, DeletionDisposition};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let _git_env = crate::session::test_support::EnvGuard::set(&[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]);
    for moved in [false, true] {
        let (_temp, _home, mut view, id, repository) = boot_view_with_dirty_worktree();
        let storage = Storage::open_unwatched("test").unwrap();
        let requested_generation = view.instances[&id].lifecycle_generation;
        let project = std::path::PathBuf::from(&view.instances[&id].project_path);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        view.deletion_poller =
            crate::tui::deletion_poller::DeletionPoller::with_handler_for_test(move |request| {
                let forced = request.force_delete;
                let result = execute_deletion(request);
                if !forced {
                    assert_eq!(result.disposition, DeletionDisposition::Failed);
                    assert!(!result.teardown_started);
                    ready_tx
                        .send(
                            result
                                .retained_instance
                                .as_ref()
                                .unwrap()
                                .lifecycle_generation,
                        )
                        .unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                } else {
                    assert_eq!(result.disposition, DeletionDisposition::Removed);
                }
                result
            });
        view.empty_trash_all(false, false);
        let first_id = view.deletes_in_flight[&id].attempt_id;
        let rolled_back_generation = ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(rolled_back_generation, requested_generation + 2);
        view.reload_storage_only().unwrap();
        assert_eq!(
            view.instances[&id].lifecycle_generation,
            rolled_back_generation
        );
        if moved {
            Storage::new_unwatched("moved").unwrap();
            let _guards = view.lock_session_mutation_and_reload(&id).unwrap();
            let current = view.instances[&id].clone();
            view.move_to_profile(&id, "moved", current, None, false)
                .unwrap();
            assert_eq!(
                view.instances[&id].lifecycle_generation,
                rolled_back_generation
            );
        }
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while view.deletes_in_flight.contains_key(&id) {
            view.apply_deletion_results();
            assert!(
                Instant::now() < deadline,
                "rollback result did not retire its ticket"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            std::fs::read(project.join("protected")).unwrap(),
            b"dirty worktree"
        );
        if moved {
            let current = &view.instances[&id];
            assert_eq!(current.source_profile, "moved");
            assert_eq!(current.lifecycle_generation, rolled_back_generation);
            assert_eq!(current.status, Status::Idle);
            assert!(current.last_error.is_none());
            assert!(view.info_dialog.is_none());
            assert!(!view.failed_deletes.contains_key(&id));
            view.save().unwrap();
            assert!(storage.load().unwrap().is_empty());
            assert_eq!(
                Storage::open_unwatched("moved").unwrap().load().unwrap()[0].status,
                Status::Idle
            );
            continue;
        }
        assert_eq!(view.instances[&id].status, Status::Error);
        assert!(view.instances[&id].last_error.is_some());
        assert_eq!(view.failed_delete_forced(&view.instances[&id]), Some(false));
        view.empty_trash_all(true, false);
        assert!(view.deletes_in_flight[&id].forced);
        assert_ne!(view.deletes_in_flight[&id].attempt_id, first_id);
        let deadline = Instant::now() + Duration::from_secs(5);
        while view.deletes_in_flight.contains_key(&id) {
            view.apply_deletion_results();
            assert!(Instant::now() < deadline, "forced retry did not complete");
            std::thread::yield_now();
        }
        assert!(!view.instances.contains_key(&id));
        assert!(storage.load().unwrap().is_empty());
        assert!(!project.exists());
        assert!(git2::Repository::open(repository.path())
            .unwrap()
            .find_branch("retry-kept", git2::BranchType::Local)
            .is_ok());
    }
}

#[test]
#[serial]
fn late_purge_and_drop_results_cannot_retire_or_modify_a_successor_purge() {
    use super::super::{operations::perform_drop, DeleteWorker};
    use crate::session::deletion::{execute_deletion, DeletionRequest};
    use crate::tui::worker::TrackedWorker;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let _git_env = crate::session::test_support::EnvGuard::set(&[
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_CONFIG_NOSYSTEM", "1"),
    ]);
    for predecessor in [DeleteWorker::Purge, DeleteWorker::Drop] {
        let (_temp, _home, mut view, id, _repository) = boot_view_with_dirty_worktree();
        let storage = Storage::open_unwatched("test").unwrap();
        let project = std::path::PathBuf::from(&view.instances[&id].project_path);
        let (ready_tx, ready_rx) = mpsc::channel();
        let (purge_tx, purge_rx) = mpsc::channel();
        let (drop_tx, drop_rx) = mpsc::channel();
        let purge_ready = ready_tx.clone();
        let mut first_purge = true;
        view.deletion_poller =
            crate::tui::deletion_poller::DeletionPoller::with_handler_for_test(move |request| {
                let is_predecessor = first_purge && predecessor == DeleteWorker::Purge;
                first_purge = false;
                if !is_predecessor {
                    purge_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                let result = execute_deletion(request);
                if is_predecessor {
                    purge_ready.send(()).unwrap();
                    purge_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                }
                result
            });
        view.drop_poller = TrackedWorker::spawn("tagged-drop-test", move |request| {
            let result = perform_drop(request);
            ready_tx.send(()).unwrap();
            drop_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            result
        });
        let queue = |view: &mut HomeView, worker| {
            let requested = view.instances[&id].clone();
            view.set_instance_status(&id, Status::Deleting);
            if worker == DeleteWorker::Drop {
                view.force_remove_session(&id).unwrap();
            } else {
                view.request_deletion(DeletionRequest {
                    session_id: id.clone(),
                    instance: requested,
                    delete_worktree: true,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: false,
                });
            }
        };
        queue(&mut view, predecessor);
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let first_id = view.deletes_in_flight[&id].attempt_id;
        view.reload_storage_only().unwrap();
        queue(&mut view, DeleteWorker::Purge);
        let next = view.deletes_in_flight[&id].clone();
        assert_ne!(next.attempt_id, first_id);
        let durable_before = std::fs::read(storage.sessions_path()).unwrap();
        if predecessor == DeleteWorker::Purge {
            purge_tx.send(()).unwrap();
        } else {
            drop_tx.send(()).unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let applied = if predecessor == DeleteWorker::Purge {
                view.apply_deletion_results()
            } else {
                view.apply_drop_results()
            };
            if applied {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "predecessor result was not delivered"
            );
            std::thread::yield_now();
        }
        assert_eq!(view.deletes_in_flight[&id].attempt_id, next.attempt_id);
        assert!(view.deletes_in_flight[&id].worker == DeleteWorker::Purge);
        assert_eq!(view.instances[&id].status, Status::Deleting);
        assert!(view.instances[&id].last_error.is_none());
        assert!(view.info_dialog.is_none());
        assert!(!view.failed_deletes.contains_key(&id));
        assert_eq!(
            std::fs::read(storage.sessions_path()).unwrap(),
            durable_before
        );
        assert_eq!(
            std::fs::read(project.join("protected")).unwrap(),
            b"dirty worktree"
        );
        purge_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while view.deletes_in_flight.contains_key(&id) {
            view.apply_deletion_results();
            assert!(
                Instant::now() < deadline,
                "successor result was not delivered"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            std::fs::read(project.join("protected")).unwrap(),
            b"dirty worktree"
        );
    }
}

#[test]
#[serial]
#[cfg(unix)]
fn failed_terminal_directory_sync_stays_failed_after_row_removal_and_reload() {
    use crate::session::deletion::{execute_deletion, DeletionDisposition};
    use crate::session::lifecycle_journal::LifecyclePhase;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    for reload in [false, true] {
        let (_temp, _home, mut view, id) =
            boot_view_with_one_session("terminal-sync", "/tmp/terminal-sync");
        let storage = Storage::open_unwatched("test").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir(&id).unwrap();
        std::fs::write(workspace.join("protected"), b"irreversible cleanup").unwrap();
        storage
            .update(|rows, _| {
                rows[0].project_path = workspace.to_string_lossy().into_owned();
                rows[0].scratch = true;
                rows[0].view = crate::session::View::Structured;
                rows[0].trash();
                Ok(())
            })
            .unwrap();
        view.reload_storage_only().unwrap();
        let failed_path = storage.sessions_path().to_path_buf();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        view.deletion_poller =
            crate::tui::deletion_poller::DeletionPoller::with_handler_for_test(move |request| {
                let _sync = crate::session::intercept_directory_sync_for_test({
                    let failed_path = failed_path.clone();
                    move |path| {
                        if path == failed_path {
                            anyhow::bail!(std::io::Error::from_raw_os_error(libc::EIO));
                        }
                        Ok(())
                    }
                });
                let result = execute_deletion(request);
                assert_eq!(result.disposition, DeletionDisposition::Failed);
                assert!(!result.success);
                assert!(result.teardown_started);
                assert!(result.retained_instance.is_none());
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                result
            });
        view.empty_trash_all(false, false);
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!workspace.exists());
        assert!(storage.load().unwrap().is_empty());
        let journal = crate::session::lifecycle_journal::path_for(
            storage.sessions_path(),
            &id,
            view.deletes_in_flight[&id].lifecycle_generation + 1,
        );
        let terminal = crate::session::lifecycle_journal::read_deletion(&journal)
            .unwrap()
            .unwrap();
        assert_eq!(terminal.phase, LifecyclePhase::TeardownComplete);
        if reload {
            view.reload_storage_only().unwrap();
        }
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while view.deletes_in_flight.contains_key(&id) {
            view.apply_deletion_results();
            assert!(Instant::now() < deadline, "terminal failure did not arrive");
            std::thread::yield_now();
        }
        assert!(
            view.info_dialog.is_some(),
            "row absence must not hide terminal durability failure"
        );
        assert!(!view.failed_deletes.contains_key(&id));
        if let Some(current) = view.instances.get(&id) {
            assert_eq!(current.status, Status::Deleting);
            assert!(current
                .last_error
                .as_deref()
                .unwrap()
                .contains("could not be made durable"));
            assert_eq!(view.failed_delete_forced(current), None);
        }
        view.empty_trash_all(true, true);
        assert!(
            !view.deletes_in_flight.contains_key(&id),
            "pending recovery must not escalate to force removal"
        );
        view.save().unwrap();
        assert!(storage.load().unwrap().is_empty());
        assert!(journal.is_file());
        assert!(crate::process::worker_registry::is_purge_fenced(&id).unwrap());
        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(!journal.exists());
        assert!(!crate::process::worker_registry::is_purge_fenced(&id).unwrap());
    }
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

#[test]
#[serial]
fn failed_native_trash_result_adopts_rollback_without_downgrading_newer_cache() {
    use crate::session::{LifecycleOperation, LifecycleReservation};
    use crate::tui::trash_poller::TrashPoller;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    for was_trashed in [false, true] {
        for newer_cache in [false, true] {
            let (temp, _home, mut view, id) =
                boot_view_with_one_session("native-trash", "/tmp/native-trash");
            let project = temp.path().join("project");
            std::fs::create_dir(&project).unwrap();
            std::fs::write(project.join("sentinel"), b"retained project").unwrap();
            let runtime = crate::process::prime::PrimeRuntime::new(
                "/bin/false".into(),
                temp.path().into(),
                None,
            )
            .unwrap();
            runtime.prepare().unwrap();
            runtime.adopt_prepared().unwrap();
            std::fs::write(runtime.directory.join("frontend.identity"), b"invalid").unwrap();
            let proof_before = std::fs::read(runtime.proof_path.as_ref().unwrap()).unwrap();
            let storage = Storage::open_unwatched("test").unwrap();
            let durable = storage
                .update(|rows, _| {
                    let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                    row.source_profile = "test".into();
                    row.project_path = project.to_string_lossy().into_owned();
                    row.status = Status::Running;
                    row.view = crate::session::View::Terminal;
                    row.active_execution = Some(crate::session::test_support::prime_execution(
                        &runtime, &project,
                    ));
                    if was_trashed {
                        row.trash();
                    }
                    Ok(row.clone())
                })
                .unwrap();
            let prior_trash = (durable.trashed_at, durable.trash_lifecycle_id.clone());
            let mut cached = durable;
            // Pre-admission trash state must come from disk, not this stale cache.
            cached.untrash();
            view.instances.insert(id.clone(), cached);
            let (ready_tx, ready_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel();
            view.trash_poller = TrashPoller::with_handler_for_test(move |request| {
                let result = crate::session::trash::perform_trash(&request);
                assert!(result.relocate_warning.is_some());
                ready_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                result
            });
            view.trash_session_by_id(&id);
            ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
            let durable = storage
                .load()
                .unwrap()
                .into_iter()
                .find(|row| row.id == id)
                .unwrap();
            let generation = durable.lifecycle_generation;
            assert_eq!(
                (durable.trashed_at, durable.trash_lifecycle_id.clone()),
                prior_trash
            );
            assert!(durable.lifecycle_reservation.is_none());
            if newer_cache {
                let cached = view.instances.get_mut(&id).unwrap();
                cached.lifecycle_generation = generation + 1;
                cached.lifecycle_reservation = Some(LifecycleReservation {
                    op: LifecycleOperation::Purge,
                    generation: generation + 1,
                    at: Utc::now(),
                });
                cached.status = Status::Deleting;
                cached.untrash();
                cached.trash();
            }
            let expected = if newer_cache {
                view.get_instance(&id).unwrap().clone()
            } else {
                durable
            };
            release_tx.send(()).unwrap();
            let deadline = Instant::now() + Duration::from_secs(3);
            while view.trash_poller.is_pending(&id) {
                view.apply_trash_results();
                assert!(Instant::now() < deadline, "trash result was not delivered");
                std::thread::yield_now();
            }
            let cached = view.get_instance(&id).unwrap();
            assert_eq!(cached.status, expected.status);
            assert_eq!(cached.lifecycle_generation, expected.lifecycle_generation);
            assert_eq!(cached.lifecycle_reservation, expected.lifecycle_reservation);
            assert_eq!(cached.trashed_at, expected.trashed_at);
            assert_eq!(cached.trash_lifecycle_id, expected.trash_lifecycle_id);
            if !newer_cache {
                view.save().unwrap();
                let saved = storage
                    .load()
                    .unwrap()
                    .into_iter()
                    .find(|row| row.id == id)
                    .unwrap();
                assert_eq!((saved.trashed_at, saved.trash_lifecycle_id), prior_trash);
                assert_eq!(saved.lifecycle_generation, generation);
                assert!(saved.lifecycle_reservation.is_none());
            }
            assert_eq!(
                std::fs::read(project.join("sentinel")).unwrap(),
                b"retained project"
            );
            assert_eq!(
                std::fs::read(runtime.directory.join("frontend.identity")).unwrap(),
                b"invalid"
            );
            assert_eq!(
                std::fs::read(runtime.proof_path.as_ref().unwrap()).unwrap(),
                proof_before
            );
        }
    }
}

#[test]
#[serial]
fn queued_trash_result_cannot_downgrade_a_failed_irreversible_purge() {
    use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
    use crate::session::LifecycleOperation;
    use crate::tui::trash_poller::{TrashPoller, TrashRequest};
    use std::sync::mpsc;
    use std::time::Duration;

    for newer_cache in [false, true] {
        let (_temp, _home, mut view, id) =
            boot_view_with_one_session("queued-trash", "/tmp/queued-trash");
        let storage = Storage::open_unwatched("test").unwrap();
        let trashed = storage
            .update(|rows, _| {
                let row = rows.iter_mut().find(|row| row.id == id).unwrap();
                row.trash();
                row.status = Status::Running;
                row.try_acquire_lifecycle_reservation(
                    LifecycleOperation::Trash,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    Utc::now(),
                )?;
                Ok(row.clone())
            })
            .unwrap();
        view.instances.insert(id.clone(), trashed.clone());
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        view.trash_poller = TrashPoller::with_handler_for_test(move |request| {
            let result = crate::session::trash::perform_trash(&request);
            assert!(
                result.relocation.is_none(),
                "plain trash must skip relocation"
            );
            assert!(
                result.relocate_warning.is_none(),
                "{:?}",
                result.relocate_warning
            );
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            result
        });
        view.trash_poller.request_trash(TrashRequest {
            session_id: id.clone(),
            generation: trashed.lifecycle_generation,
            instance: trashed,
            was_trashed: false,
        });
        ready_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let current = storage.load().unwrap().remove(0);
        let transaction = match PurgeTransaction::reserve_with_acp_transcript(
            storage.clone(),
            DeletionRequest {
                session_id: id.clone(),
                instance: current,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: true,
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("unexpected rejection: {result:?}"),
        };
        let failure = transaction
            .run_hooks()
            .begin_irreversible()
            .unwrap()
            .finish_with_transcript_cleanup(|_| anyhow::bail!("transcript unavailable"));
        assert!(failure.teardown_started && !failure.success);
        let durable = storage.load().unwrap().remove(0);
        let generation = durable.lifecycle_generation;
        let journal =
            crate::session::lifecycle_journal::path_for(storage.sessions_path(), &id, generation);
        let journal_before = std::fs::read(&journal).unwrap();
        if newer_cache {
            let cached = view.instances.get_mut(&id).unwrap();
            cached.lifecycle_generation = generation + 1;
            cached.lifecycle_reservation = None;
            cached.status = Status::Stopped;
        }
        release_tx.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while view.trash_poller.is_pending(&id) {
            view.apply_trash_results();
            assert!(
                std::time::Instant::now() < deadline,
                "trash result was not delivered"
            );
            std::thread::yield_now();
        }
        let cached = view.get_instance(&id).unwrap();
        if newer_cache {
            assert_eq!(cached.lifecycle_generation, generation + 1);
            assert_eq!(cached.status, Status::Stopped);
            assert!(cached.lifecycle_reservation.is_none());
        } else {
            assert_eq!(cached.status, Status::Deleting);
            assert!(cached.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
            view.save().unwrap();
            let saved = storage.load().unwrap().remove(0);
            assert_eq!(saved.status, Status::Deleting);
            assert!(saved.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
        }
        assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
    }
}

#[test]
#[serial]
fn stale_removed_purge_result_retires_its_ticket_without_erasing_a_durable_successor() {
    use crate::session::deletion::{execute_deletion, DeletionDisposition, DeletionRequest};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};
    let (_temp, _home, mut view, id) = boot_view_with_one_session("successor", "/tmp/successor");
    let storage = Storage::open_unwatched("test").unwrap();
    let requested = view.instances[&id].clone();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    view.deletion_poller =
        crate::tui::deletion_poller::DeletionPoller::with_handler_for_test(move |request| {
            let result = execute_deletion(request);
            assert_eq!(result.disposition, DeletionDisposition::Removed);
            ready_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            result
        });
    view.request_deletion(DeletionRequest {
        session_id: id.clone(),
        instance: requested.clone(),
        delete_worktree: false,
        delete_branch: false,
        delete_sandbox: false,
        force_delete: false,
        detach_hooks: true,
        keep_scratch: false,
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(storage.load().unwrap().is_empty());
    let mut successor = requested;
    successor.lifecycle_generation = 9;
    successor.status = Status::Stopped;
    storage
        .update(|rows, _| {
            rows.push(successor);
            Ok(())
        })
        .unwrap();
    view.reload_storage_only().unwrap();
    release_tx.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !view.apply_deletion_results() {
        assert!(
            Instant::now() < deadline,
            "successful purge result did not arrive"
        );
        std::thread::yield_now();
    }
    assert!(!view.deletes_in_flight.contains_key(&id));
    let successor = view
        .get_instance(&id)
        .expect("stale success erased successor");
    assert_eq!(successor.lifecycle_generation, 9);
    assert_eq!(successor.status, Status::Stopped);
    assert!(view.info_dialog.is_none());
    assert!(!view.failed_deletes.contains_key(&id));
}

#[test]
#[serial]
fn failed_terminal_checkpoint_survives_tui_result_application_and_save() {
    use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};

    for delivery in ["retained", "missing", "stale", "force-after-result"] {
        let (temp, _guard, mut view, id) = boot_view_with_one_session("pending", "/tmp/pending");
        let storage = Storage::open_unwatched("test").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir(&id).unwrap();
        std::fs::write(workspace.join("retained"), b"pending teardown").unwrap();
        storage
            .update(|rows, _| {
                rows[0].project_path = workspace.to_string_lossy().into_owned();
                rows[0].scratch = true;
                Ok(())
            })
            .unwrap();
        let instance = storage.load().unwrap().remove(0);
        let requested = instance.clone();
        let attempt_id = 1;
        view.next_delete_attempt_id = attempt_id;
        view.deletes_in_flight.insert(
            id.clone(),
            super::super::DeleteAttempt {
                attempt_id,
                source_profile: requested.source_profile.clone(),
                worker: super::super::DeleteWorker::Purge,
                lifecycle_generation: requested.lifecycle_generation,
                forced: false,
                trashed_at: requested.trashed_at,
                trash_lifecycle_id: requested.trash_lifecycle_id.clone(),
            },
        );
        let transaction = match PurgeTransaction::reserve_with_acp_transcript(
            storage.clone(),
            DeletionRequest {
                session_id: id.clone(),
                instance,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: false,
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("unexpected rejection: {result:?}"),
        };
        let observed = storage.load().unwrap().remove(0);
        let generation = observed.lifecycle_generation;
        view.instances.insert(id.clone(), observed);
        let journal =
            crate::session::lifecycle_journal::path_for(storage.sessions_path(), &id, generation);
        let saved_journal = temp.path().join("checkpoint-before-terminal.json");
        let mut result = transaction
            .run_hooks()
            .begin_irreversible()
            .unwrap()
            .finish_with_transcript_cleanup(|_| {
                std::fs::rename(&journal, &saved_journal)?;
                std::fs::create_dir(&journal)?;
                Ok(())
            });
        assert!(!result.success);
        assert!(
            !workspace.exists(),
            "teardown completed before the checkpoint failed"
        );
        assert_eq!(
            result.retained_instance.as_ref().unwrap().status,
            Status::Deleting
        );
        if delivery == "missing" {
            result.retained_instance = None;
        }
        if delivery == "stale" {
            let current = view.instances.get_mut(&id).unwrap();
            current.lifecycle_generation += 1;
            current.lifecycle_reservation = None;
            current.status = Status::Running;
            current.last_error = Some("successor runtime error".into());
            view.deletes_in_flight.insert(
                id.clone(),
                super::super::DeleteAttempt {
                    attempt_id: 2,
                    source_profile: "test".into(),
                    worker: super::super::DeleteWorker::Purge,
                    lifecycle_generation: generation + 1,
                    forced: false,
                    trashed_at: None,
                    trash_lifecycle_id: None,
                },
            );
            view.next_delete_attempt_id = 2;
        }
        std::fs::remove_dir(&journal).unwrap();
        std::fs::rename(saved_journal, &journal).unwrap();
        if delivery == "force-after-result" {
            let before = std::fs::read(&journal).unwrap();
            let error = view.force_remove_session(&id).unwrap_err();
            assert!(error.to_string().contains("pending deletion"));
            assert_eq!(view.deletes_in_flight[&id].attempt_id, attempt_id);
            assert!(view.deletes_in_flight[&id].worker == super::super::DeleteWorker::Purge);
            assert_eq!(view.next_delete_attempt_id, attempt_id);
            assert!(!view.drop_poller.is_pending(&id));
            assert_eq!(std::fs::read(&journal).unwrap(), before);
        }
        view.deletion_poller =
            crate::tui::deletion_poller::DeletionPoller::seeded_for_test(attempt_id, result);

        assert!(view.apply_deletion_results());
        if delivery == "force-after-result" {
            assert!(!view.deletes_in_flight.contains_key(&id));
            assert_eq!(view.instances[&id].status, Status::Deleting);
            assert!(view.instances[&id].lifecycle_reservation_is_owned(
                crate::session::LifecycleOperation::Purge,
                generation,
            ));
            assert!(view.info_dialog.is_some());
            assert!(view.instances[&id].last_error.is_some());
            let retained = storage.load().unwrap().remove(0);
            assert_eq!(retained.status, Status::Deleting);
            assert!(retained.lifecycle_reservation_is_owned(
                crate::session::LifecycleOperation::Purge,
                generation,
            ));
            assert!(journal.is_file());
            view.force_remove_session(&id).unwrap();
            assert!(view.deletes_in_flight[&id].attempt_id > attempt_id);
            assert!(view.deletes_in_flight[&id].worker == super::super::DeleteWorker::Drop);
            assert!(!view.failed_deletes.contains_key(&id));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
            while !view.apply_drop_results() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "drop result did not arrive"
                );
                std::thread::yield_now();
            }
            assert!(!view.instances.contains_key(&id));
            assert!(!view.deletes_in_flight.contains_key(&id));
            let abandoned = crate::session::lifecycle_journal::read_deletion(&journal)
                .unwrap()
                .unwrap();
            assert_eq!(
                abandoned.phase,
                crate::session::lifecycle_journal::LifecyclePhase::Abandoned
            );
            assert_eq!(abandoned.generation, generation);
            crate::session::deletion::recover_lifecycle_journals_once().unwrap();
            assert!(!journal.exists());
            continue;
        }
        if delivery == "stale" {
            let current = &view.instances[&id];
            assert_eq!(current.status, Status::Running);
            assert_eq!(current.lifecycle_generation, generation + 1);
            assert!(current.lifecycle_reservation.is_none());
            assert_eq!(
                current.last_error.as_deref(),
                Some("successor runtime error")
            );
            assert!(view.info_dialog.is_none());
            assert!(!view.failed_deletes.contains_key(&id));
            continue;
        }
        assert!(
            view.info_dialog.is_some(),
            "retained deletion failure must be visible"
        );
        view.save().unwrap();

        let retained = storage.load().unwrap().remove(0);
        assert_eq!(retained.status, Status::Deleting);
        assert!(
            retained.lifecycle_reservation_is_owned(
                crate::session::LifecycleOperation::Purge,
                generation,
            )
        );
        assert!(journal.exists());
        assert!(view.instances[&id].last_error.is_some());
        storage
            .update(|rows, _| {
                rows[0].lifecycle_reservation.as_mut().unwrap().at =
                    Utc::now() - chrono::Duration::days(1);
                Ok(())
            })
            .unwrap();
        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(storage.load().unwrap().is_empty());
        assert!(!journal.exists());
    }
}

#[test]
#[serial]
fn disconnected_delete_worker_keeps_purge_authority_and_other_workers_isolated() {
    use super::super::DeleteWorker;
    use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
    use crate::session::LifecycleOperation;

    for owned_purge in [true, false] {
        let (temp, _guard, mut view, id) =
            boot_view_with_one_session("crashed-delete", "/tmp/crashed-delete");
        let storage = Storage::open_unwatched("test").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir(&id).unwrap();
        std::fs::write(workspace.join("retained"), b"await recovery").unwrap();
        storage
            .update(|rows, _| {
                rows[0].project_path = workspace.to_string_lossy().into_owned();
                rows[0].scratch = true;
                Ok(())
            })
            .unwrap();
        let request = |instance: Instance| DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        };
        let reserve = |instance| match PurgeTransaction::reserve(storage.clone(), request(instance))
            .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(result) => panic!("unexpected rejection: {result:?}"),
        };
        if owned_purge {
            let transaction = reserve(storage.load().unwrap().remove(0));
            drop(transaction.run_hooks().begin_irreversible().unwrap());
        }
        let mut current = storage.load().unwrap().remove(0);
        current.status = Status::Deleting;
        view.instances.insert(id.clone(), current);

        let peer_path = temp.path().join("peer-project");
        std::fs::create_dir(&peer_path).unwrap();
        let mut peer = Instance::new("peer-purge", peer_path.to_str().unwrap());
        peer.source_profile = "test".into();
        let peer_id = peer.id.clone();
        let drop_path = temp.path().join("drop-project");
        std::fs::create_dir(&drop_path).unwrap();
        let mut drop_row = Instance::new("drop-worker", drop_path.to_str().unwrap());
        drop_row.source_profile = "test".into();
        drop_row.status = Status::Deleting;
        let drop_id = drop_row.id.clone();
        storage
            .update(|rows, _| {
                rows.extend([peer.clone(), drop_row.clone()]);
                Ok(())
            })
            .unwrap();
        let peer_transaction = reserve(peer);
        let peer = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == peer_id)
            .unwrap();
        view.instances.insert(peer_id.clone(), peer);
        view.instances.insert(drop_id.clone(), drop_row);
        view.force_remove_session(&drop_id).unwrap();
        view.deletion_poller = crate::tui::deletion_poller::DeletionPoller::crashing_for_test();
        view.request_deletion(request(view.instances[&id].clone()));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !view.apply_deletion_results() {
            assert!(
                std::time::Instant::now() < deadline,
                "worker failure was not reported"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            view.instances[&id].status,
            if owned_purge {
                Status::Deleting
            } else {
                Status::Error
            }
        );
        assert!(view.instances[&id].last_error.is_some());
        assert!(view.info_dialog.is_some());
        assert_eq!(view.instances[&peer_id].status, Status::Deleting);
        assert!(view.instances[&peer_id].last_error.is_none());
        assert!(!view.failed_deletes.contains_key(&peer_id));
        assert!(matches!(
            view.deletes_in_flight[&drop_id].worker,
            DeleteWorker::Drop
        ));
        assert!(!view.failed_deletes.contains_key(&drop_id));
        view.info_dialog = None;
        assert!(!view.apply_deletion_results());
        assert!(view.info_dialog.is_none());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !view.apply_drop_results() {
            assert!(
                std::time::Instant::now() < deadline,
                "healthy drop worker did not finish"
            );
            std::thread::yield_now();
        }
        assert!(!view.instances.contains_key(&drop_id));
        assert!(!view.deletes_in_flight.contains_key(&drop_id));
        assert_eq!(view.instances[&peer_id].status, Status::Deleting);
        assert!(view.instances[&peer_id].last_error.is_none());
        drop(peer_transaction);

        view.save().unwrap();
        let retained = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == id)
            .unwrap();
        if owned_purge {
            assert_eq!(retained.status, Status::Deleting);
            assert!(retained.lifecycle_reservation_is_owned(
                LifecycleOperation::Purge,
                retained.lifecycle_generation
            ));
            storage
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == id)
                        .unwrap()
                        .lifecycle_reservation
                        .as_mut()
                        .unwrap()
                        .at = Utc::now() - chrono::Duration::days(1);
                    Ok(())
                })
                .unwrap();
            crate::session::deletion::recover_lifecycle_journals_once().unwrap();
            assert!(!storage.load().unwrap().iter().any(|row| row.id == id));
            assert!(!workspace.exists());
        } else {
            assert_eq!(retained.status, Status::Error);
            assert!(workspace.join("retained").exists());
        }
    }
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
    view.move_to_profile(&id, "target", requested, None, false)
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
    view.move_to_profile(&id, "test", requested, None, false)
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
    view.move_to_profile(&id, "target", requested, None, false)
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
    view.move_to_profile(&id, "target", requested, None, false)
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
        .move_to_profile(&id, "target", requested, None, false)
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
    view.move_to_profile(&id, "target", requested, Some(&baseline), false)
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

/// `stamp_last_accessed` on a sunk row must auto-clear archived_at in memory and on disk
/// and rebuild flat_items, so the row leaves the Archived section on the same frame. The old
/// mutate_instance + save path left it stuck until `z`, because merge_from_tui doesn't carry
/// archived_at and the next reload resurrected the sink.
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

    // Snoozed sibling: `snoozed_until` is also excluded from `merge_from_tui`.
    view.apply_user_action(&id, |inst| inst.snooze(30))
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

    view.restart_selected_session(Some("target"), Some("claude-2"), None, None)
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

fn failed_transcript_purge_for_cache_test(storage: &Storage, instance: Instance) -> Instance {
    use crate::session::deletion::{DeletionRequest, PurgeReservation, PurgeTransaction};
    let transaction = match PurgeTransaction::reserve_with_acp_transcript(
        storage.clone(),
        DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: true,
        },
    )
    .unwrap()
    {
        PurgeReservation::Reserved(transaction) => transaction,
        PurgeReservation::Rejected(result) => panic!("unexpected purge rejection: {result:?}"),
    };
    let failure = transaction
        .run_hooks()
        .begin_irreversible()
        .unwrap()
        .finish_with_transcript_cleanup(|_| anyhow::bail!("transcript unavailable"));
    assert!(!failure.success && failure.teardown_started);
    storage.load().unwrap().remove(0)
}

#[test]
#[serial]
fn stop_failure_and_disconnect_preserve_durable_purge_authority() {
    use crate::session::LifecycleOperation;
    use crate::tui::stop_poller::{StopPoller, StopRequest};
    use std::sync::mpsc;
    use std::time::Duration;

    for crash in [false, true] {
        let (_temp, _home, mut view, id) =
            boot_view_with_one_session("stale-stop", "/tmp/stale-stop-result");
        let storage = Storage::open_unwatched("test").unwrap();
        let stale = view.get_instance(&id).unwrap().clone();
        let durable = failed_transcript_purge_for_cache_test(&storage, stale.clone());
        let generation = durable.lifecycle_generation;
        let reservation = durable.lifecycle_reservation.clone();
        let journal =
            crate::session::lifecycle_journal::path_for(storage.sessions_path(), &id, generation);
        let journal_before = std::fs::read(&journal).unwrap();
        view.instances.get_mut(&id).unwrap().status = Status::Stopped;
        let (entered_tx, entered_rx) = mpsc::channel();
        view.stop_poller = StopPoller::with_handler_for_test(move |request| {
            let result = crate::session::stop::perform_stop(&request);
            assert!(!result.success, "Stop must refuse a committed purge");
            entered_tx.send(()).unwrap();
            assert!(!crash, "simulated stop worker failure");
            result
        });
        view.stop_poller.request_stop(StopRequest {
            session_id: id.clone(),
            instance: stale,
        });
        entered_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !view.apply_stop_results() {
            assert!(
                std::time::Instant::now() < deadline,
                "Stop outcome was not delivered"
            );
            std::thread::yield_now();
        }
        let cached = view.get_instance(&id).unwrap();
        assert_eq!(cached.status, Status::Deleting);
        assert!(cached.lifecycle_reservation_is_owned(LifecycleOperation::Purge, generation));
        view.save().unwrap();
        let saved = storage.load().unwrap().remove(0);
        assert_eq!(saved.status, Status::Deleting);
        assert_eq!(saved.lifecycle_generation, generation);
        assert_eq!(saved.lifecycle_reservation, reservation);
        assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
    }
}

#[test]
#[serial]
fn profile_move_preserves_expired_purge_authority_but_allows_expired_launch() {
    use crate::session::LifecycleOperation;
    use std::cell::Cell;
    for case in ["stale-purge", "reloaded-purge", "expired-launch"] {
        let (_temp, _home, mut view, id) =
            boot_view_with_one_session("move-authority", "/tmp/move-authority");
        let source = Storage::open_unwatched("test").unwrap();
        let target = Storage::new_unwatched("target").unwrap();
        target.update(|_, _| Ok(())).unwrap();
        view.storages.insert("target".into(), target.clone());
        let baseline = view.get_instance(&id).unwrap().clone();
        let purging = case != "expired-launch";
        if purging {
            let durable = failed_transcript_purge_for_cache_test(&source, baseline.clone());
            crate::process::worker_registry::fence_for_purge(
                &id,
                "test",
                durable.lifecycle_generation,
            )
            .unwrap();
        } else {
            source
                .update(|rows, _| {
                    rows[0].try_acquire_lifecycle_reservation(
                        LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )?;
                    Ok(())
                })
                .unwrap();
        }
        source
            .update(|rows, _| {
                rows[0].lifecycle_reservation.as_mut().unwrap().at =
                    Utc::now() - chrono::Duration::days(1);
                Ok(())
            })
            .unwrap();
        let durable = source.load().unwrap().remove(0);
        let journal = crate::session::lifecycle_journal::path_for(
            source.sessions_path(),
            &id,
            durable.lifecycle_generation,
        );
        let fence = crate::process::worker_registry::workers_dir()
            .unwrap()
            .join(format!("{id}.purge-fence"));
        let source_before = std::fs::read(source.sessions_path()).unwrap();
        let target_before = std::fs::read(target.sessions_path()).unwrap();
        let journal_before = std::fs::read(&journal).ok();
        let fence_before = std::fs::read(&fence).ok();
        let _guards = if case == "reloaded-purge" {
            Some(view.lock_session_mutation_and_reload(&id).unwrap())
        } else {
            None
        };
        let mut requested = baseline.clone();
        requested.source_profile = "target".into();
        let effect_called = Cell::new(false);
        let result = view.move_to_profile_with_effect(
            &id,
            "target",
            requested,
            Some(&baseline),
            false,
            |_| {
                effect_called.set(true);
                Ok(())
            },
        );
        if purging {
            let error = result.expect_err("profile move replaced expired purge authority");
            assert!(error
                .downcast_ref::<crate::session::StartBlocked>()
                .is_some_and(|blocked| *blocked == crate::session::StartBlocked::Deleting));
            assert!(!effect_called.get());
            assert_eq!(
                std::fs::read(source.sessions_path()).unwrap(),
                source_before
            );
            assert_eq!(
                std::fs::read(target.sessions_path()).unwrap(),
                target_before
            );
            assert_eq!(std::fs::read(&journal).ok(), journal_before);
            assert_eq!(std::fs::read(&fence).ok(), fence_before);
            let retained = source.load().unwrap().remove(0);
            assert_eq!(retained.status, Status::Deleting);
            assert!(retained.lifecycle_reservation_is_owned(
                LifecycleOperation::Purge,
                durable.lifecycle_generation,
            ));
        } else {
            result.unwrap();
            assert!(effect_called.get());
            assert!(source.load().unwrap().is_empty());
            let moved = target.load().unwrap().remove(0);
            assert_eq!(moved.id, id);
            assert_eq!(moved.source_profile, "target");
        }
    }
}
