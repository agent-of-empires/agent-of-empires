use super::*;
use crate::session::move_journal;

#[test]
#[serial_test::serial]
fn git_discovery_releases_root_and_revalidates_late_branch_conflicts() {
    for late_ref in [false, true] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (directory, storage, row) = fixture();
        let gate = crate::session::test_support::GitCommandGate::new(
            directory.path(),
            "[ \"$1\" = show-ref ]",
            true,
        );
        let request = edit_request(&row, "both", false);
        let worker = std::thread::spawn(move || perform(request, |_, _| Ok(())));
        let entered = gate.wait_entered();
        let root_free = storage::try_acquire_ownership_lock().unwrap().is_some();
        if entered && root_free && late_ref {
            let repo = git2::Repository::open(&row.worktree_info.as_ref().unwrap().main_repo_path)
                .unwrap();
            repo.branch(
                "new",
                &repo.head().unwrap().peel_to_commit().unwrap(),
                false,
            )
            .unwrap();
        }
        gate.release();
        let result = worker.join().unwrap();
        assert!(entered, "rename did not reach show-ref discovery");
        assert!(root_free, "discovery retained Root");
        if late_ref {
            let error = result.err().expect("late branch must reject admission");
            assert!(matches!(
                error.downcast_ref::<super::super::worktree_edit::WorktreeEditError>(),
                Some(super::super::worktree_edit::WorktreeEditError::BranchExists(_))
            ));
            assert_eq!(storage.load().unwrap()[0].project_path, row.project_path);
        } else {
            let committed = result.unwrap();
            assert!(Path::new(&committed.instance.project_path).exists());
        }
        assert!(move_journal::scan().entries.is_empty());
    }
}

fn fixture() -> (tempfile::TempDir, Storage, Instance) {
    let root = tempfile::tempdir().unwrap();
    let repo_path = root.path().join("repo");
    let repo = git2::Repository::init(&repo_path).unwrap();
    std::fs::write(repo_path.join("tracked"), "base").unwrap();
    let mut index = repo.index().unwrap();
    index.add_path(Path::new("tracked")).unwrap();
    let tree = index.write_tree().unwrap();
    let signature = git2::Signature::now("fixture", "fixture@example.invalid").unwrap();
    repo.commit(
        Some("HEAD"),
        &signature,
        &signature,
        "base",
        &repo.find_tree(tree).unwrap(),
        &[],
    )
    .unwrap();
    let base = repo.head().unwrap().shorthand().unwrap().to_string();
    let path = root.path().join("old");
    crate::git::GitWorktree::new(repo_path.clone())
        .unwrap()
        .with_init_submodules(false)
        .create_worktree("topic", &path, true, Some(&base))
        .unwrap();
    let storage = Storage::new_unwatched("owner").unwrap();
    let mut row = Instance::new("old", &path.to_string_lossy());
    row.source_profile = "owner".into();
    row.status = crate::session::Status::Stopped;
    row.view = crate::session::View::Structured;
    row.worktree_info = Some(crate::session::WorktreeInfo {
        branch: "topic".into(),
        main_repo_path: repo_path.to_string_lossy().into_owned(),
        managed_by_aoe: true,
        created_at: Utc::now(),
        base_branch: Some(base),
    });
    storage
        .update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })
        .unwrap();
    (root, storage, row)
}

fn request(row: &Instance, kind: &str) -> Request {
    Request {
        baseline: row.clone(),
        edit: if kind == "workdir" {
            Edit::Workdir {
                name: "new".into(),
                rename_branch: true,
            }
        } else {
            Edit::Session {
                title: Some("new".into()),
                group: None,
                profile: (kind == "profile").then(|| "target".into()),
                rename_branch: true,
                branch: None,
            }
        },
        file_watch: crate::file_watch::FileWatchService::noop(),
    }
}

#[test]
#[serial_test::serial]
fn preflight_releases_all_flocks_and_preserves_unsubmitted_peer_fields() {
    for kind in ["workdir", "same", "profile"] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(kind != "workdir");
        let (root, storage, row) = fixture();
        let target = Storage::new_unwatched("target").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let id = row.id.clone();
        let source = storage.clone();
        let competing = request(&row, kind);
        let mut writer = None;
        let committed = perform(request(&row, kind), |reserved, moves| {
            assert!(moves);
            assert!(reserved.lifecycle_reservation_is_owned(
                LifecycleOperation::Rename,
                reserved.lifecycle_generation
            ));
            let (done, completed) = std::sync::mpsc::channel();
            writer = Some(std::thread::spawn(move || {
                let refused = perform(competing, |_, _| {
                    panic!("reserved rename must not enter preflight")
                })
                .err()
                .expect("another rename must reject the persisted reservation");
                assert!(matches!(
                    refused.downcast_ref::<crate::session::LifecycleReservationError>(),
                    Some(crate::session::LifecycleReservationError::Busy(
                        LifecycleOperation::Rename
                    ))
                ));
                let root = storage::acquire_ownership_read().unwrap();
                let _identity =
                    storage::acquire_session_identity_lock_with_ownership(&root).unwrap();
                let _title =
                    storage::acquire_session_title_lock_with_ownership(&root, &id).unwrap();
                let _lifecycle = source
                    .acquire_instance_lifecycle_lock_with_ownership(&root, &id)
                    .unwrap();
                peer.update_with_ownership(&root, |rows, _| {
                    rows.push(Instance::new("peer identity", "/unrelated"));
                    Ok(())
                })
                .unwrap();
                source
                    .update_with_ownership(&root, |rows, _| {
                        rows.iter_mut().find(|row| row.id == id).unwrap().group_path =
                            "peer-group".into();
                        Ok(())
                    })
                    .unwrap();
                let _ = done.send(());
            }));
            completed
                .recv_timeout(std::time::Duration::from_millis(500))
                .context("identity/lifecycle writer stayed blocked during preflight")
        });
        writer.unwrap().join().unwrap();
        let committed = committed.unwrap();
        let durable = if kind == "profile" {
            target.load().unwrap()
        } else {
            storage.load().unwrap()
        };
        let durable = durable.iter().find(|current| current.id == row.id).unwrap();
        assert_eq!(durable.created_at, row.created_at);
        assert_eq!(durable.group_path, "peer-group");
        assert!(durable.lifecycle_reservation.is_none());
        assert_eq!(
            durable.project_path,
            root.path().join("new").to_string_lossy()
        );
        assert_eq!(durable.worktree_info.as_ref().unwrap().branch, "new");
        assert_eq!(committed.instance.project_path, durable.project_path);
        assert!(root.path().join("new/tracked").exists());
        assert!(!root.path().join("old").exists());
        if kind == "profile" {
            assert!(storage.load().unwrap().is_empty());
        }
    }
}

#[test]
#[serial_test::serial]
fn reserved_rename_refuses_competing_namespace_mutations_and_commits_owner() {
    for change in [
        "generation",
        "incarnation",
        "source-profile",
        "target-profile",
        "destination",
        "source",
    ] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (root, storage, row) = fixture();
        let target = Storage::new_unwatched("target").unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let committed = perform(request(&row, "profile"), |reserved, _| {
            let refused = match change {
                "generation" | "incarnation" => storage.update(|rows, _| {
                    let current = rows
                        .iter_mut()
                        .find(|current| current.id == row.id)
                        .unwrap();
                    current.release_lifecycle_reservation_if_owned(
                        LifecycleOperation::Rename,
                        reserved.lifecycle_generation,
                    );
                    current.try_acquire_lifecycle_reservation(
                        LifecycleOperation::Launch,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        Utc::now(),
                    )?;
                    if change == "incarnation" {
                        current.created_at += chrono::Duration::seconds(1);
                    }
                    Ok(())
                }),
                "source-profile" | "target-profile" => {
                    let profile = if change == "source-profile" {
                        storage.profile()
                    } else {
                        target.profile()
                    };
                    assert!(crate::session::delete_profile(profile).is_err());
                    crate::session::rename_profile(profile, "retired-profile")
                }
                "destination" | "source" => peer.update(|rows, _| {
                    let path = if change == "source" {
                        root.path().join("old")
                    } else {
                        root.path().join("new")
                    };
                    rows.push(Instance::new("adopter", &path.to_string_lossy()));
                    Ok(())
                }),
                _ => unreachable!(),
            };
            assert!(refused.is_err(), "{change} bypassed the rename reservation");
            let current = storage.load_strict_for_worktree_ownership()?.remove(0);
            assert_eq!(current.id, reserved.id, "{change}");
            assert_eq!(current.created_at, reserved.created_at, "{change}");
            assert_eq!(current.project_path, reserved.project_path, "{change}");
            assert_eq!(
                current.lifecycle_generation, reserved.lifecycle_generation,
                "{change}"
            );
            assert_eq!(
                current.lifecycle_reservation, reserved.lifecycle_reservation,
                "{change}"
            );
            assert!(current.lifecycle_reservation_is_owned(
                LifecycleOperation::Rename,
                reserved.lifecycle_generation
            ));
            assert!(current
                .lifecycle_reservation
                .as_ref()
                .unwrap()
                .resource_intent
                .is_some());
            assert!(target.load_strict_for_worktree_ownership()?.is_empty());
            assert!(peer.load_strict_for_worktree_ownership()?.is_empty());
            assert!(root.path().join("old/tracked").exists(), "{change}");
            assert!(!root.path().join("new").exists(), "{change}");
            Ok(())
        })
        .unwrap();
        assert!(storage.load().unwrap().is_empty(), "{change}");
        let durable = target.load().unwrap();
        assert_eq!(durable.len(), 1, "{change}");
        let current = &durable[0];
        assert_eq!(current.id, row.id, "{change}");
        assert_eq!(current.created_at, row.created_at, "{change}");
        assert_eq!(
            current.lifecycle_generation,
            row.lifecycle_generation + 1,
            "{change}"
        );
        assert!(current.lifecycle_reservation.is_none(), "{change}");
        assert_eq!(current.source_profile, "target");
        assert_eq!(
            current.project_path,
            root.path().join("new").to_string_lossy()
        );
        assert_eq!(current.worktree_info.as_ref().unwrap().branch, "new");
        assert_eq!(committed.instance.id, current.id);
        assert_eq!(committed.instance.project_path, current.project_path);
        assert!(peer.load().unwrap().is_empty(), "{change}");
        assert!(root.path().join("new/tracked").exists(), "{change}");
        assert!(!root.path().join("old").exists(), "{change}");
        let repo = git2::Repository::open(&current.project_path).unwrap();
        assert_eq!(repo.head().unwrap().shorthand().unwrap(), "new");
        assert!(repo.find_reference("refs/heads/topic").is_err());
        assert!(move_journal::scan().entries.is_empty());
    }
}

#[test]
#[serial_test::serial]
fn competing_git_branch_defeats_rename_without_moving_checkout() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, storage, row) = fixture();
    let target = Storage::new_unwatched("target").unwrap();
    let result = perform(request(&row, "profile"), |_, _| {
        let repo = git2::Repository::open(root.path().join("repo"))?;
        repo.branch("new", &repo.head()?.peel_to_commit()?, false)?;
        Ok(())
    });
    assert!(result.is_err());
    assert!(root.path().join("old/tracked").exists());
    assert!(!root.path().join("new").exists());
    let durable = storage.load().unwrap();
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].id, row.id);
    assert_eq!(durable[0].created_at, row.created_at);
    assert_eq!(durable[0].project_path, row.project_path);
    assert_eq!(durable[0].worktree_info, row.worktree_info);
    assert!(target.load().unwrap().is_empty());
    let repo = git2::Repository::open(&row.project_path).unwrap();
    assert_eq!(repo.head().unwrap().shorthand().unwrap(), "topic");
    assert!(repo.find_reference("refs/heads/topic").is_ok());
    assert!(repo.find_reference("refs/heads/new").is_ok());
}

#[test]
#[serial_test::serial]
fn metadata_rename_uses_canonical_profile_for_an_unstamped_request() {
    let _app = crate::session::test_support::isolate_app_dir();
    let storage = Storage::new_unwatched("default").unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut row = Instance::new("before", &directory.path().to_string_lossy());
    row.view = crate::session::View::Structured;
    row.group_path = "kept".into();
    storage
        .update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })
        .unwrap();
    let mut raw: Vec<Instance> =
        serde_json::from_slice(&std::fs::read(storage.sessions_path()).unwrap()).unwrap();
    let baseline = raw.remove(0);
    assert!(baseline.source_profile.is_empty());
    assert!(baseline.worktree_info.is_none());
    let committed = perform(
        Request {
            baseline,
            edit: Edit::Session {
                title: Some("after".into()),
                group: None,
                profile: None,
                rename_branch: false,
                branch: None,
            },
            file_watch: crate::file_watch::FileWatchService::noop(),
        },
        |reserved, moves| {
            assert_eq!(reserved.source_profile, "default");
            assert!(!moves);
            Ok(())
        },
    )
    .unwrap();
    for instance in [
        &committed.baseline,
        &committed.original,
        &committed.instance,
    ] {
        assert_eq!(instance.source_profile, "default");
        assert_eq!(instance.id, row.id);
        assert_eq!(instance.created_at, row.created_at);
    }
    let durable = storage.load().unwrap().remove(0);
    assert_eq!(durable.title, "after");
    assert_eq!(durable.project_path, row.project_path);
    assert_eq!(durable.group_path, "kept");
    assert_eq!(durable.lifecycle_generation, row.lifecycle_generation + 1);
    assert!(durable.lifecycle_reservation.is_none());
    assert_eq!(committed.instance.title, durable.title);
    assert_eq!(
        committed.instance.lifecycle_generation,
        durable.lifecycle_generation
    );
}

#[test]
#[serial_test::serial]
fn unchanged_tied_title_still_refuses_a_claimed_derived_workdir() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, storage, mut row) = fixture();
    row.title = "new".into();
    let claimed = root.path().join("new");
    storage
        .update(|rows, _| {
            rows[0].title.clone_from(&row.title);
            rows.push(Instance::new("new", &claimed.to_string_lossy()));
            Ok(())
        })
        .unwrap();
    let error = perform(
        Request {
            baseline: row.clone(),
            edit: Edit::Session {
                title: Some("new".into()),
                group: None,
                profile: None,
                rename_branch: false,
                branch: None,
            },
            file_watch: crate::file_watch::FileWatchService::noop(),
        },
        |_, _| panic!("duplicate destination must refuse before unlocked preflight"),
    )
    .err()
    .expect("unchanged title must still validate its derived path");
    assert!(matches!(
        error.downcast_ref::<RenameRejected>(),
        Some(RenameRejected::Duplicate(_))
    ));
    let durable = storage.load().unwrap();
    let original = durable.iter().find(|current| current.id == row.id).unwrap();
    assert_eq!(original.project_path, row.project_path);
    assert_eq!(original.lifecycle_generation, row.lifecycle_generation);
    assert!(original.lifecycle_reservation.is_none());
    assert!(root.path().join("old/tracked").exists());
    assert!(!claimed.exists());
}

fn edit_request(row: &Instance, kind: &str, cross_profile: bool) -> Request {
    Request {
        baseline: row.clone(),
        edit: Edit::Session {
            title: (kind != "branch").then(|| "new".into()),
            group: None,
            profile: cross_profile.then(|| "target".into()),
            rename_branch: kind == "both",
            branch: (kind == "branch").then(|| "exact/Requested-Branch".into()),
        },
        file_watch: crate::file_watch::FileWatchService::noop(),
    }
}

#[test]
#[serial_test::serial]
fn real_git_effects_recover_after_same_and_cross_profile_publication_failure() {
    for cross_profile in [false, true] {
        for kind in ["branch", "move", "both"] {
            let _app = crate::session::test_support::isolate_app_dir();
            let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
            let (root, source, before) = fixture();
            let target = Storage::new_unwatched("target").unwrap();
            target.update(|_, _| Ok(())).unwrap();
            let failed = if cross_profile {
                target.clone()
            } else {
                source.clone()
            };
            let file = failed.sessions_path().to_path_buf();
            let saved = root.path().join("original-sessions.json");
            let restore_file = file.clone();
            let restore_saved = saved.clone();
            let result = with_after_effects(
                move |_, _, candidate| {
                    assert!(Path::new(&candidate.project_path).join("tracked").exists());
                    let repo = git2::Repository::open(&candidate.project_path).unwrap();
                    assert_eq!(
                        repo.head().unwrap().shorthand().unwrap(),
                        candidate.worktree_info.as_ref().unwrap().branch.as_str()
                    );
                    std::fs::rename(&file, &saved).unwrap();
                    std::fs::create_dir(&file).unwrap();
                },
                || perform(edit_request(&before, kind, cross_profile), |_, _| Ok(())),
            );
            assert!(
                result.is_err(),
                "{kind}/{cross_profile}: real publication failure must surface"
            );
            let pending = move_journal::scan();
            assert_eq!(pending.entries.len(), 1);
            std::fs::remove_dir(&restore_file).unwrap();
            std::fs::rename(&restore_saved, &restore_file).unwrap();
            assert!(
                Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
                    .unwrap()
                    .is_empty()
            );
            assert!(move_journal::scan().entries.is_empty());
            let (winner, loser) = if cross_profile {
                (&target, &source)
            } else {
                (&source, &target)
            };
            let durable = winner.load().unwrap();
            assert_eq!(durable.len(), 1);
            let current = &durable[0];
            let expected_path = if kind == "branch" {
                root.path().join("old")
            } else {
                root.path().join("new")
            };
            let expected_branch = match kind {
                "branch" => "exact/Requested-Branch",
                "move" => "topic",
                "both" => "new",
                _ => unreachable!(),
            };
            assert_eq!(current.id, before.id);
            assert_eq!(current.created_at, before.created_at);
            assert_eq!(
                current.lifecycle_generation,
                before.lifecycle_generation + 1
            );
            assert!(current.lifecycle_reservation.is_none());
            assert_eq!(current.source_profile, winner.profile());
            assert_eq!(current.title, if kind == "branch" { "old" } else { "new" });
            assert_eq!(current.project_path, expected_path.to_string_lossy());
            let mut expected_info = before.worktree_info.clone();
            expected_info.as_mut().unwrap().branch = expected_branch.into();
            assert_eq!(current.worktree_info, expected_info);
            assert!(loser.load().unwrap().is_empty());
            assert_eq!(
                std::fs::read(expected_path.join("tracked")).unwrap(),
                b"base"
            );
            let repo = git2::Repository::open(&expected_path).unwrap();
            assert_eq!(repo.head().unwrap().shorthand().unwrap(), expected_branch);
            let common =
                git2::Repository::open(&before.worktree_info.as_ref().unwrap().main_repo_path)
                    .unwrap();
            assert_eq!(
                common.find_reference("refs/heads/topic").is_ok(),
                kind == "move"
            );
            assert_eq!(
                common.find_reference("refs/heads/new").is_ok(),
                kind == "both"
            );
            assert_eq!(
                common
                    .find_reference("refs/heads/exact/Requested-Branch")
                    .is_ok(),
                kind == "branch"
            );
            assert_eq!(root.path().join("old").exists(), kind == "branch");
            assert_eq!(root.path().join("new").exists(), kind != "branch");
            assert!(
                Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
                    .unwrap()
                    .is_empty()
            );
            let recovered_again = winner.load().unwrap();
            assert_eq!(recovered_again.len(), 1);
            assert_eq!(recovered_again[0].id, current.id);
            assert_eq!(recovered_again[0].created_at, current.created_at);
            assert_eq!(recovered_again[0].project_path, current.project_path);
            assert_eq!(recovered_again[0].worktree_info, current.worktree_info);
            assert_eq!(
                recovered_again[0].lifecycle_generation,
                current.lifecycle_generation
            );
            assert!(recovered_again[0].lifecycle_reservation.is_none());
            assert!(loser.load().unwrap().is_empty());
            assert!(move_journal::scan().entries.is_empty());
        }
    }
}

#[test]
#[serial_test::serial]
fn post_effect_resource_reservation_rejects_supersession_and_commits_owner() {
    for change in ["generation", "incarnation", "checkout", "destination-owner"] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (root, source, before) = fixture();
        let peer = Storage::new_unwatched("peer").unwrap();
        let edited = source.clone();
        let adopter = peer.clone();
        let result = with_after_effects(
            move |ownership, original, candidate| {
                let refused = if change == "destination-owner" {
                    adopter.update_with_ownership(ownership, |rows, _| {
                        rows.push(Instance::new("new owner", &candidate.project_path));
                        Ok(())
                    })
                } else {
                    edited.update_with_ownership(ownership, |rows, _| {
                        let row = rows.iter_mut().find(|row| row.id == original.id).unwrap();
                        match change {
                            "generation" => {
                                row.release_lifecycle_reservation_if_owned(
                                    LifecycleOperation::Rename,
                                    original.lifecycle_generation,
                                );
                                row.try_acquire_lifecycle_reservation(
                                    LifecycleOperation::Trash,
                                    Instance::LIFECYCLE_RESERVATION_TTL,
                                    Utc::now(),
                                )?;
                            }
                            "incarnation" => {
                                row.created_at += chrono::Duration::seconds(1);
                            }
                            "checkout" => {
                                row.project_path.clone_from(&candidate.project_path);
                                row.worktree_info = None;
                            }
                            _ => unreachable!(),
                        }
                        Ok(())
                    })
                };
                assert!(
                    refused.is_err(),
                    "{change}: resource supersession was accepted"
                );
            },
            || perform(edit_request(&before, "both", false), |_, _| Ok(())),
        );
        let committed = result.unwrap_or_else(|error| panic!("{change}: {error:#}"));
        assert_eq!(committed.instance.created_at, before.created_at);
        assert_eq!(
            committed.instance.project_path,
            root.path().join("new").to_string_lossy()
        );
        assert_eq!(
            committed.instance.worktree_info.as_ref().unwrap().branch,
            "new"
        );
        assert!(root.path().join("new/tracked").exists(), "{change}");
        assert!(!root.path().join("old").exists(), "{change}");
        assert!(peer.load().unwrap().is_empty(), "{change}");
        assert!(move_journal::scan().entries.is_empty(), "{change}");
        let durable = source.load().unwrap().remove(0);
        assert_eq!(durable.project_path, committed.instance.project_path);
        assert_eq!(durable.worktree_info, committed.instance.worktree_info);
        assert!(durable.lifecycle_reservation.is_none());
        let repo =
            git2::Repository::open(&before.worktree_info.as_ref().unwrap().main_repo_path).unwrap();
        assert!(repo.find_reference("refs/heads/topic").is_err(), "{change}");
        assert!(repo.find_reference("refs/heads/new").is_ok(), "{change}");
    }
}

#[test]
#[serial_test::serial]
fn terminal_rename_consumes_fresh_status_and_preserves_late_active_status() {
    for kind in ["workdir", "same"] {
        for durable_status in [
            super::super::Status::Running,
            super::super::Status::Waiting,
            super::super::Status::Starting,
            super::super::Status::Stopped,
        ] {
            let _app = crate::session::test_support::isolate_app_dir();
            let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(kind != "workdir");
            let (root, storage, mut row) = fixture();
            let _tmux = crate::session::test_support::EnvGuard::set(&[(
                "AOE_TMUX_SOCKET",
                root.path().join("absent-tmux.sock"),
            )]);
            row.view = crate::session::View::Terminal;
            row.status = durable_status;
            storage
                .update(|rows, _| {
                    rows[0] = row.clone();
                    Ok(())
                })
                .unwrap();
            let committed = perform(request(&row, kind), |_, _| Ok(())).unwrap();
            assert!(root.path().join("new/tracked").exists());
            assert!(!root.path().join("old").exists());
            let latest = storage.load().unwrap().remove(0);
            assert!(!latest.status.blocks_worktree_edit());
            assert_eq!(latest.id, row.id);
            assert_eq!(latest.project_path, committed.instance.project_path);
            assert!(latest.lifecycle_reservation.is_none());
            assert!(move_journal::scan().entries.is_empty());
        }
    }

    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, storage, mut row) = fixture();
    let _tmux = crate::session::test_support::EnvGuard::set(&[(
        "AOE_TMUX_SOCKET",
        root.path().join("absent-tmux.sock"),
    )]);
    row.view = crate::session::View::Terminal;
    row.status = super::super::Status::Idle;
    storage
        .update(|rows, _| {
            rows[0] = row.clone();
            Ok(())
        })
        .unwrap();
    let result = perform(request(&row, "same"), |_, _| {
        storage.update(|rows, _| {
            rows[0].status = super::super::Status::Running;
            rows[0].group_path = "concurrent annotation".into();
            Ok(())
        })
    });
    assert!(result.is_err());
    assert!(root.path().join("old/tracked").exists());
    assert!(!root.path().join("new").exists());
    let latest = storage.load().unwrap().remove(0);
    assert_eq!(latest.status, super::super::Status::Running);
    assert_eq!(latest.group_path, "concurrent annotation");
}

#[test]
#[serial_test::serial]
fn terminal_rename_keeps_creating_deleting_and_fresh_launch_gates() {
    for gate in ["creating", "deleting", "launch"] {
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (root, storage, mut row) = fixture();
        row.view = crate::session::View::Terminal;
        row.status = match gate {
            "creating" => super::super::Status::Creating,
            "deleting" => super::super::Status::Deleting,
            _ => super::super::Status::Stopped,
        };
        if gate == "launch" {
            row.try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                Utc::now(),
            )
            .unwrap();
        }
        storage
            .update(|rows, _| {
                rows[0] = row.clone();
                Ok(())
            })
            .unwrap();
        assert!(perform(request(&row, "same"), |_, _| panic!(
            "admission must refuse"
        ))
        .is_err());
        assert!(root.path().join("old/tracked").exists());
        assert!(!root.path().join("new").exists());
        assert!(move_journal::scan().entries.is_empty());
    }
}

#[test]
#[serial_test::serial]
#[serial_test::serial(hook_base)]
fn live_terminal_is_probed_even_when_saved_status_is_nonblocking() {
    if !crate::tmux::tmux_command()
        .arg("-V")
        .output()
        .is_ok_and(|out| out.status.success())
    {
        return;
    }
    for status in [
        super::super::Status::Stopped,
        super::super::Status::Idle,
        super::super::Status::Error,
    ] {
        let (_hooks, _base, _directory) = crate::hooks::test_support::BaseGuard::ready();
        let _app = crate::session::test_support::isolate_app_dir();
        let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
        let (root, storage, mut row) = fixture();
        let _tmux = crate::session::test_support::EnvGuard::set(&[(
            "AOE_TMUX_SOCKET",
            root.path().join("live-tmux.sock"),
        )]);
        row.view = crate::session::View::Terminal;
        row.tool = "settl".into();
        row.status = status;
        let name = crate::tmux::Session::generate_name(&row.id, &row.title);
        struct KillSession(String);
        impl Drop for KillSession {
            fn drop(&mut self) {
                let _ = crate::tmux::tmux_command()
                    .args(["kill-session", "-t", &self.0])
                    .output();
            }
        }
        let _cleanup = KillSession(name.clone());
        let created = crate::tmux::tmux_command()
            .args(["new-session", "-d", "-s", &name, "sleep 60"])
            .output()
            .unwrap();
        assert!(
            created.status.success(),
            "{}",
            String::from_utf8_lossy(&created.stderr)
        );
        let hooks = crate::hooks::hook_status_dir(&row.id).unwrap();
        std::fs::create_dir_all(&hooks).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(hooks.join("status"), "running").unwrap();
        storage
            .update(|rows, _| {
                rows[0] = row.clone();
                Ok(())
            })
            .unwrap();
        row.last_error = Some("probe cooldown".into());
        row.last_error_check = Some(std::time::Instant::now());
        assert!(
            observe_terminal_preflight(&row).is_err(),
            "{status:?}: stale status bypassed live probe"
        );
        assert!(
            perform(request(&row, "same"), |_, _| Ok(())).is_err(),
            "{status:?}"
        );
        assert!(root.path().join("old/tracked").exists());
        assert!(!root.path().join("new").exists());
        assert_eq!(storage.load().unwrap()[0].status, status);
    }
}

#[test]
#[serial_test::serial]
fn rename_final_gate_preserves_status_published_after_git() {
    let _app = crate::session::test_support::isolate_app_dir();
    let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(true);
    let (root, storage, row) = fixture();
    let writer = storage.clone();
    let result = with_after_effects(
        move |ownership, original, _| {
            writer
                .update_with_ownership(ownership, |rows, _| {
                    let current = rows.iter_mut().find(|row| row.id == original.id).unwrap();
                    current.status = super::super::Status::Running;
                    current.group_path = "late annotation".into();
                    Ok(())
                })
                .unwrap();
        },
        || perform(request(&row, "same"), |_, _| Ok(())),
    );
    assert!(result.is_err());
    assert!(root.path().join("old/tracked").exists());
    assert!(!root.path().join("new").exists());
    let latest = storage.load().unwrap().remove(0);
    assert_eq!(latest.status, super::super::Status::Running);
    assert_eq!(latest.group_path, "late annotation");
}

#[test]
#[serial_test::serial]
fn structured_rename_preserves_live_baseline_gate_when_disk_status_is_idle() {
    for kind in ["workdir", "same"] {
        for active in [
            crate::session::Status::Running,
            crate::session::Status::Waiting,
        ] {
            let _app = crate::session::test_support::isolate_app_dir();
            let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(kind != "workdir");
            let (root, storage, mut row) = fixture();
            row.status = crate::session::Status::Idle;
            storage
                .update(|rows, _| {
                    rows[0] = row.clone();
                    Ok(())
                })
                .unwrap();
            let mut baseline = row.clone();
            baseline.status = active;
            let error = perform(request(&baseline, kind), |_, _| {
                panic!("live structured baseline must reject before callback")
            })
            .err()
            .expect("live structured session must reject");
            assert!(matches!(
                error.downcast_ref::<RenameRejected>(),
                Some(RenameRejected::Active)
            ));
            assert!(root.path().join("old/tracked").exists());
            assert!(!root.path().join("new").exists());
            let durable = storage.load().unwrap().remove(0);
            assert_eq!(durable.status, crate::session::Status::Idle);
            assert_eq!(durable.lifecycle_generation, row.lifecycle_generation);
            assert!(durable.lifecycle_reservation.is_none());
            let git = git2::Repository::open(&row.project_path).unwrap();
            assert_eq!(git.head().unwrap().shorthand().unwrap(), "topic");
            assert!(git.find_reference("refs/heads/new").is_err());
            assert!(move_journal::scan().entries.is_empty());
        }
    }
}
