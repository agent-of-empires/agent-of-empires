//! Profile-directory commit and durable lifecycle-reference settlement.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::lifecycle_journal::{self, LifecycleJournalEntry};
use super::storage::{atomic_write_verified, sync_parent_directory, StorageFlock};
use super::Storage;

const CHECKPOINT: &str = ".profile-rename.json";

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct DirectoryIdentity {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(not(unix))]
    created: std::time::SystemTime,
}

impl DirectoryIdentity {
    fn read(path: &Path) -> Result<Option<Self>> {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        anyhow::ensure!(metadata.is_dir(), "profile is not a directory");
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            }))
        }
        #[cfg(not(unix))]
        {
            Ok(Some(Self {
                created: metadata.created()?,
            }))
        }
    }
}

#[derive(Serialize, Deserialize)]
struct RenameCheckpoint {
    version: u32,
    old: String,
    new: String,
    directory: DirectoryIdentity,
    session_ids: Vec<String>,
    default_was_old: bool,
}

fn checkpoint_path() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join(CHECKPOINT))
}

fn read_checkpoint() -> Result<Option<RenameCheckpoint>> {
    let bytes = match fs::read(checkpoint_path()?) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let checkpoint: RenameCheckpoint = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        checkpoint.version == 1,
        "unsupported profile rename checkpoint"
    );
    super::validate_profile_name(&checkpoint.old)?;
    super::validate_new_profile_name(&checkpoint.new)?;
    anyhow::ensure!(checkpoint.old != checkpoint.new, "invalid rename identity");
    for id in &checkpoint.session_ids {
        super::validate_instance_id(id)?;
    }
    anyhow::ensure!(
        checkpoint
            .session_ids
            .windows(2)
            .all(|pair| pair[0] < pair[1]),
        "unordered rename owners"
    );
    Ok(Some(checkpoint))
}

pub(crate) fn ensure_profile_available(profile: &str) -> Result<()> {
    if let Some(checkpoint) = read_checkpoint()? {
        anyhow::ensure!(
            profile != checkpoint.old && profile != checkpoint.new,
            "profile rename remains pending: {} -> {}",
            checkpoint.old,
            checkpoint.new
        );
    }
    Ok(())
}

pub(crate) fn publication_guard(profile: Option<&str>, id: &str) -> Result<impl Drop> {
    let guard = super::storage::acquire_storage_shared_flock(
        &super::get_app_dir()?,
        crate::migrations::v027_isolate_sandbox_stores::LOCK,
    )?;
    if let Some(profile) = profile {
        ensure_profile_available(profile)?;
        let storage = Storage::open_unwatched(profile)?;
        let row = storage
            .load()?
            .into_iter()
            .find(|row| row.id == id)
            .context("runner session no longer belongs to its source profile")?;
        row.ensure_startable().map_err(anyhow::Error::msg)?;
    }
    Ok(guard)
}

fn entries(storage: &Storage) -> Result<Vec<(PathBuf, LifecycleJournalEntry)>> {
    let scan = lifecycle_journal::scan([storage.sessions_path().to_path_buf()]);
    anyhow::ensure!(
        scan.unreadable_dirs.is_empty(),
        "profile journals cannot be enumerated"
    );
    scan.entries
        .into_iter()
        .map(|(path, entry)| {
            let entry = entry.map_err(anyhow::Error::msg)?;
            let key = lifecycle_journal::filename_key(&path)?;
            anyhow::ensure!(
                key.session_id() == entry.session_id
                    && key.generation == entry.generation
                    && entry.instance.id == entry.session_id,
                "rename journal identity mismatch"
            );
            Ok((path, entry))
        })
        .collect()
}

fn owner_guards(storage: &Storage, ids: &[String]) -> Result<Vec<StorageFlock>> {
    let mut guards = Vec::with_capacity(ids.len() * 2);
    for id in ids {
        guards.push(
            storage
                .try_acquire_purge_owner_lock(id)?
                .context("profile has a live purge owner")?,
        );
        guards.push(
            storage
                .try_acquire_instance_lifecycle_lock(id)?
                .context("profile has a live lifecycle owner")?,
        );
    }
    Ok(guards)
}

fn retarget(storage: &Storage, checkpoint: &RenameCheckpoint, target: &str) -> Result<()> {
    let old_sessions = super::get_profile_dir_path(&checkpoint.old)?.join("sessions.json");
    let new_sessions = super::get_profile_dir_path(&checkpoint.new)?.join("sessions.json");
    let target_sessions = if target == checkpoint.old {
        &old_sessions
    } else {
        &new_sessions
    };
    for (path, mut entry) in entries(storage)? {
        anyhow::ensure!(
            checkpoint
                .session_ids
                .binary_search(&entry.session_id)
                .is_ok(),
            "unexpected rename journal owner"
        );
        anyhow::ensure!(
            (entry.source_profile == checkpoint.old || entry.source_profile == checkpoint.new)
                && (entry.sessions_path == old_sessions || entry.sessions_path == new_sessions),
            "foreign rename journal reference"
        );
        if entry.source_profile != target || entry.sessions_path != *target_sessions {
            entry.source_profile = target.to_owned();
            entry.instance.source_profile = target.to_owned();
            entry.sessions_path = target_sessions.clone();
            lifecycle_journal::update(&path, &entry)?;
        }
    }
    #[cfg(test)]
    super::storage::test_crash_point("profile-rename-after-journals");
    for profile in [&checkpoint.old, &checkpoint.new] {
        for id in crate::process::worker_registry::profile_reference_sessions(profile)? {
            anyhow::ensure!(
                checkpoint.session_ids.binary_search(&id).is_ok(),
                "unexpected rename registry owner"
            );
        }
    }
    for id in &checkpoint.session_ids {
        crate::process::worker_registry::retarget_profile_references(
            id,
            &checkpoint.old,
            &checkpoint.new,
            target,
        )?;
    }
    #[cfg(test)]
    super::storage::test_crash_point("profile-rename-after-registry");
    Ok(())
}

fn finish_default(checkpoint: &RenameCheckpoint) -> Result<()> {
    if checkpoint.default_was_old {
        super::update_config(|config| {
            if config.default_profile == checkpoint.old {
                config.default_profile.clone_from(&checkpoint.new);
            }
        })?;
    }
    Ok(())
}

fn settle(checkpoint: &RenameCheckpoint) -> Result<()> {
    let old_dir = super::get_profile_dir_path(&checkpoint.old)?;
    let new_dir = super::get_profile_dir_path(&checkpoint.new)?;
    let old = DirectoryIdentity::read(&old_dir)?;
    let new = DirectoryIdentity::read(&new_dir)?;
    let (profile, moved) = match (old.as_ref(), new.as_ref()) {
        (Some(identity), None) if identity == &checkpoint.directory => (&checkpoint.old, false),
        (None, Some(identity)) if identity == &checkpoint.directory => (&checkpoint.new, true),
        _ => anyhow::bail!("profile rename directory identity is ambiguous"),
    };
    let storage = Storage::open_unwatched(profile)?;
    storage.with_profile_rename_locked(|| {
        let _guards = owner_guards(&storage, &checkpoint.session_ids)?;
        retarget(&storage, checkpoint, profile)?;
        sync_parent_directory(
            storage
                .sessions_path()
                .parent()
                .context("profile directory absent")?,
        )?;
        if moved {
            finish_default(checkpoint)?;
        }
        lifecycle_journal::consume(&checkpoint_path()?)
    })
}

pub(crate) fn recover_once() -> Result<()> {
    if !checkpoint_path()?.try_exists()? {
        return Ok(());
    }
    let Some(_identity) = super::storage::try_acquire_session_identity_lock()? else {
        return Ok(());
    };
    if let Some(checkpoint) = read_checkpoint()? {
        settle(&checkpoint)?;
    }
    Ok(())
}

pub(crate) fn rename(old: &str, new: &str) -> Result<()> {
    let _identity = super::storage::acquire_session_identity_lock()?;
    if let Some(checkpoint) = read_checkpoint()? {
        settle(&checkpoint)?;
    }
    let old_dir = super::get_profile_dir_path(old)?;
    let new_dir = super::get_profile_dir_path(new)?;
    let storage = Storage::open_unwatched(old)?;
    let result = storage.with_profile_rename_locked(|| {
        anyhow::ensure!(!new_dir.try_exists()?, "Profile '{}' already exists", new);
        let directory = DirectoryIdentity::read(&old_dir)?
            .with_context(|| format!("Profile '{old}' does not exist"))?;
        let mut ids: Vec<String> = storage.load()?.into_iter().map(|row| row.id).collect();
        for (_, entry) in entries(&storage)? {
            anyhow::ensure!(
                entry.source_profile == old && entry.sessions_path == storage.sessions_path(),
                "foreign profile journal"
            );
            ids.push(entry.session_id);
        }
        ids.extend(crate::process::worker_registry::profile_reference_sessions(
            old,
        )?);
        ids.sort();
        ids.dedup();
        let _guards = owner_guards(&storage, &ids)?;
        let checkpoint = RenameCheckpoint {
            version: 1,
            old: old.to_owned(),
            new: new.to_owned(),
            directory,
            session_ids: ids,
            default_was_old: super::load_config()?
                .is_some_and(|config| config.default_profile == old),
        };
        let path = checkpoint_path()?;
        atomic_write_verified(&path, &serde_json::to_vec(&checkpoint)?)?;
        sync_parent_directory(&path)?;
        retarget(&storage, &checkpoint, new)?;
        #[cfg(unix)]
        {
            let parent: std::os::fd::OwnedFd =
                fs::File::open(old_dir.parent().context("profiles directory absent")?)?.into();
            crate::process::rename_exclusive(
                &parent,
                std::ffi::OsStr::new(old),
                &parent,
                std::ffi::OsStr::new(new),
            )?;
        }
        #[cfg(not(unix))]
        fs::rename(&old_dir, &new_dir)?;
        #[cfg(test)]
        super::storage::test_crash_point("profile-rename-after-directory");
        sync_parent_directory(&new_dir)?;
        finish_default(&checkpoint)?;
        lifecycle_journal::consume(&path)
    });
    if let Err(error) = result {
        if let Some(checkpoint) = read_checkpoint()? {
            if let Err(settlement) = settle(&checkpoint) {
                return Err(
                    error.context(format!("rename settlement remains pending: {settlement:#}"))
                );
            }
        }
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{Instance, LifecycleOperation, Status, View};
    use serial_test::serial;
    #[test]
    #[serial]
    fn concurrent_destination_creation_refuses_rename_without_poisoning_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = Storage::new_unwatched("before").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir("rename-race").unwrap();
        fs::write(workspace.join("retained"), b"recoverable purge").unwrap();
        let mut row = Instance::new("race", workspace.to_str().unwrap());
        row.id = "rename-race".into();
        row.source_profile = "before".into();
        row.scratch = true;
        let generation = row
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Purge,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now() - chrono::Duration::days(1),
            )
            .unwrap();
        row.status = Status::Deleting;
        storage
            .update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })
            .unwrap();
        lifecycle_journal::record(
            &LifecycleJournalEntry::deletion(
                row,
                Status::Idle,
                storage.sessions_path().to_path_buf(),
                lifecycle_journal::LifecycleDeletionOptions {
                    generation,
                    delete_worktree: false,
                    delete_branch: false,
                    delete_sandbox: false,
                    force_delete: false,
                    detach_hooks: true,
                    keep_scratch: false,
                    purge_acp_transcript: false,
                },
            )
            .with_phase(lifecycle_journal::LifecyclePhase::TeardownStarted),
        )
        .unwrap();
        let transition = super::super::storage::acquire_storage_shared_flock(
            &super::super::get_app_dir().unwrap(),
            crate::migrations::v027_isolate_sandbox_stores::LOCK,
        )
        .unwrap();
        let (queued_tx, queued_rx) = std::sync::mpsc::channel();
        let renamer = std::thread::spawn(move || {
            let _observer = super::super::storage::observe_lock_contention_for_test(queued_tx);
            super::super::rename_profile("before", "after")
        });
        queued_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        super::super::create_profile("after").unwrap();
        let target = super::super::get_profile_dir_path("after").unwrap();
        fs::write(
            target.join("foreign-data"),
            b"concurrent profile stays untouched",
        )
        .unwrap();
        drop(transition);
        assert!(renamer.join().unwrap().is_err());
        super::super::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(
            !workspace.exists(),
            "failed rename poisoned unrelated purge recovery"
        );
        assert!(storage.load().unwrap().is_empty());
        assert_eq!(
            fs::read(target.join("foreign-data")).unwrap(),
            b"concurrent profile stays untouched"
        );
        assert!(
            !checkpoint_path().unwrap().exists(),
            "ordinary conflict left a pending rename"
        );
    }

    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[cfg(unix)]
    #[test]
    #[serial]
    fn interrupted_rename_settles_before_purge_and_cannot_recreate_or_delete_its_profile() {
        use std::os::unix::process::CommandExt as _;
        for point in [
            "profile-rename-after-journals",
            "profile-rename-after-registry",
            "profile-rename-after-directory",
        ] {
            for rowless in [false, true] {
                let temp = tempfile::TempDir::with_prefix_in("aoe-", "/tmp").unwrap();
                let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
                let storage = Storage::new_unwatched("before").unwrap();
                let witness = Storage::new_unwatched("witness").unwrap();
                let mut witness_row = Instance::new("move witness", temp.path().to_str().unwrap());
                witness_row.id = "move-witness".into();
                witness_row.source_profile = "witness".into();
                witness
                    .update(|rows, _| {
                        rows.push(witness_row.clone());
                        Ok(())
                    })
                    .unwrap();
                let workspace = crate::session::scratch::provision_scratch_dir("renamed").unwrap();
                std::fs::write(
                    workspace.join("retained"),
                    b"live runner still owns scratch",
                )
                .unwrap();
                let mut child = OwnedChild(
                    std::process::Command::new("sh")
                        .args(["-c", "exec sleep 60"])
                        .process_group(0)
                        .spawn()
                        .unwrap(),
                );
                let pid = child.0.id();
                let mut row = Instance::new("renamed", workspace.to_str().unwrap());
                row.id = "renamed".into();
                row.source_profile = "before".into();
                row.view = View::Structured;
                row.scratch = true;
                let generation = row
                    .try_acquire_lifecycle_reservation(
                        LifecycleOperation::Purge,
                        Instance::LIFECYCLE_RESERVATION_TTL,
                        chrono::Utc::now() - chrono::Duration::days(1),
                    )
                    .unwrap();
                row.status = Status::Deleting;
                if !rowless {
                    storage
                        .update(|rows, _| {
                            rows.push(row.clone());
                            Ok(())
                        })
                        .unwrap();
                }
                let entry = LifecycleJournalEntry::deletion(
                    row,
                    Status::Idle,
                    storage.sessions_path().to_path_buf(),
                    lifecycle_journal::LifecycleDeletionOptions {
                        generation,
                        delete_worktree: false,
                        delete_branch: false,
                        delete_sandbox: false,
                        force_delete: false,
                        detach_hooks: true,
                        keep_scratch: false,
                        purge_acp_transcript: false,
                    },
                )
                .with_phase(lifecycle_journal::LifecyclePhase::TeardownStarted);
                lifecycle_journal::record(&entry).unwrap();
                let record = crate::process::worker_registry::WorkerRecord::new(
                    "renamed".into(),
                    pid,
                    crate::process::worker_registry::socket_path_for("renamed").unwrap(),
                    "fixture".into(),
                    "fixture".into(),
                    workspace.clone(),
                    None,
                    vec![],
                    vec![],
                    None,
                    Some("before".into()),
                )
                .with_generation(5);
                crate::process::worker_registry::save(&record).unwrap();
                crate::process::worker_registry::fence_for_purge("renamed", "before", generation)
                    .unwrap();
                let orphan_path = crate::process::worker_registry::workers_dir()
                    .unwrap()
                    .join("renamed.orphan-stop");
                std::fs::write(&orphan_path, serde_json::to_vec(&record).unwrap()).unwrap();
                crate::session::set_default_profile("before").unwrap();
                let interruption = {
                    let _point = super::super::storage::ArmedCrashPoint::arm(point);
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        crate::session::rename_profile("before", "after")
                    }))
                };
                assert!(interruption.is_err());
                assert!(checkpoint_path().unwrap().exists());
                let moved = point == "profile-rename-after-directory";
                let original = if moved { "after" } else { "before" };
                for name in ["before", "after"] {
                    assert!(crate::session::get_profile_dir(name).is_err());
                    assert!(crate::session::delete_profile(name).is_err());
                }
                assert!(storage
                    .update(|rows, _| {
                        rows.clear();
                        Ok(())
                    })
                    .is_err());
                let pending_target = Storage::open_unwatched(original).unwrap();
                let checkpoint_before = fs::read(checkpoint_path().unwrap()).unwrap();
                let witness_before = fs::read(witness.sessions_path()).unwrap();
                let target_before = if rowless {
                    assert!(!pending_target.sessions_path().exists());
                    None
                } else {
                    Some(fs::read(pending_target.sessions_path()).unwrap())
                };
                let targets = if moved {
                    vec![&storage, &pending_target]
                } else {
                    vec![&storage]
                };
                let group_move = crate::session::GroupMovePlan::single(
                    &witness_row.group_path,
                    &witness_row.group_path,
                );
                for target in targets {
                    assert!(witness
                        .move_instances_to(
                            target,
                            &[(witness_row.clone(), witness_row.clone())],
                            &group_move,
                            |_, _| panic!("pending rename reached move validation"),
                        )
                        .is_err());
                    assert!(witness
                        .move_instance_to_with_effect(
                            target,
                            &witness_row,
                            &witness_row,
                            false,
                            |_, _| panic!("pending rename reached move validation"),
                            |_| panic!("pending rename reached external effect"),
                        )
                        .is_err());
                }
                assert_eq!(
                    fs::read(checkpoint_path().unwrap()).unwrap(),
                    checkpoint_before
                );
                assert_eq!(fs::read(witness.sessions_path()).unwrap(), witness_before);
                match target_before {
                    Some(bytes) => {
                        assert_eq!(fs::read(pending_target.sessions_path()).unwrap(), bytes)
                    }
                    None => assert!(!pending_target.sessions_path().exists()),
                }
                assert_eq!(
                    crate::session::get_profile_dir_path("before")
                        .unwrap()
                        .exists(),
                    !moved
                );
                assert_eq!(
                    crate::session::get_profile_dir_path("after")
                        .unwrap()
                        .exists(),
                    moved
                );
                assert!(workspace.join("retained").exists());
                if moved {
                    crate::session::set_default_profile("witness").unwrap();
                }
                crate::session::deletion::recover_lifecycle_journals_once().unwrap();
                assert!(!checkpoint_path().unwrap().exists());
                assert!(workspace.join("retained").exists());
                let target = Storage::open_unwatched(original).unwrap();
                if !rowless {
                    assert_eq!(target.load().unwrap()[0].status, Status::Deleting);
                }
                assert_eq!(
                    crate::session::load_config()
                        .unwrap()
                        .unwrap()
                        .default_profile,
                    if moved { "witness" } else { "before" }
                );
                let migrated = crate::process::worker_registry::load("renamed")
                    .unwrap()
                    .unwrap();
                assert_eq!(migrated.source_profile.as_deref(), Some(original));
                assert_eq!(migrated.process_identity, record.process_identity);
                assert_eq!(fs::read(witness.sessions_path()).unwrap(), witness_before);
                let transferred = witness
                    .move_instances_to(
                        &target,
                        &[(witness_row.clone(), witness_row.clone())],
                        &group_move,
                        |_, _| Ok(()),
                    )
                    .unwrap()
                    .remove(0);
                assert!(witness.load().unwrap().is_empty());
                assert!(target
                    .load()
                    .unwrap()
                    .iter()
                    .any(|row| row.id == witness_row.id));
                target
                    .move_instance_to_with_effect(
                        &witness,
                        &transferred,
                        &transferred,
                        false,
                        |_, _| Ok(()),
                        |_| Ok(()),
                    )
                    .unwrap();
                assert_eq!(fs::read(witness.sessions_path()).unwrap(), witness_before);
                assert!(target
                    .load()
                    .unwrap()
                    .iter()
                    .all(|row| row.id != "move-witness"));
                child.0.kill().unwrap();
                child.0.wait().unwrap();
                if !rowless {
                    target
                        .update(|rows, _| {
                            rows[0].lifecycle_reservation.as_mut().unwrap().at =
                                chrono::Utc::now() - chrono::Duration::days(1);
                            Ok(())
                        })
                        .unwrap();
                }
                crate::session::deletion::recover_lifecycle_journals_once().unwrap();
                assert!(
                    !workspace.exists(),
                    "{point}, rowless={rowless}: settled journal did not complete purge"
                );
                assert!(target.load().unwrap().is_empty());
                assert!(!orphan_path.exists());
                assert!(!crate::process::worker_registry::is_purge_fenced("renamed").unwrap());
            }
        }
    }
}
