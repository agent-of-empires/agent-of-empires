//! Profile-directory commit and durable lifecycle-reference settlement.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::process::DirectoryIdentity;

use super::lifecycle_journal::{self, LifecycleJournalEntry, LifecyclePhase};
use super::storage::{atomic_write_verified, sync_parent_directory, StorageFlock};
use super::Storage;

const CHECKPOINT: &str = ".profile-rename.json";
const DELETE_CHECKPOINT: &str = ".profile-delete.json";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameCheckpoint {
    version: u32,
    old: String,
    new: String,
    directory: DirectoryIdentity,
    session_ids: Vec<String>,
    move_journals: Vec<MoveJournalSnapshot>,
    default_was_old: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveJournalSnapshot {
    directory_profile: String,
    file_name: String,
    identity: DirectoryIdentity,
    entry: super::move_journal::MoveJournalEntry,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum DeletePhase {
    Prepared,
    Committed,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteCheckpoint {
    version: u32,
    profile: String,
    directory: DirectoryIdentity,
    tombstone: PathBuf,
    survivor: String,
    default_was_deleted: bool,
    phase: DeletePhase,
}

fn delete_checkpoint_path() -> Result<PathBuf> {
    Ok(super::get_app_dir()?.join(DELETE_CHECKPOINT))
}

fn read_delete_checkpoint() -> Result<Option<DeleteCheckpoint>> {
    let bytes = match fs::read(delete_checkpoint_path()?) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let checkpoint: DeleteCheckpoint = serde_json::from_slice(&bytes)?;
    anyhow::ensure!(
        checkpoint.version == 1,
        "unsupported profile delete checkpoint"
    );
    super::validate_profile_name(&checkpoint.profile)?;
    super::validate_profile_name(&checkpoint.survivor)?;
    anyhow::ensure!(
        checkpoint.profile != checkpoint.survivor,
        "invalid profile delete survivor"
    );
    let profiles = super::get_app_dir()?.join("profiles");
    anyhow::ensure!(
        checkpoint.tombstone.parent() == Some(profiles.as_path())
            && checkpoint
                .tombstone
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(super::PROFILE_DELETE_TOMBSTONE_PREFIX)),
        "invalid profile delete tombstone"
    );
    Ok(Some(checkpoint))
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
    for snapshot in &checkpoint.move_journals {
        super::validate_profile_name(&snapshot.directory_profile)?;
        let name_path = Path::new(&snapshot.file_name);
        anyhow::ensure!(
            name_path
                .file_name()
                .is_some_and(|name| name == snapshot.file_name.as_str())
                && snapshot.file_name.ends_with(".json")
                && super::move_journal::file_created_at_nanos(name_path).is_some(),
            "invalid rename move journal filename"
        );
        let entry = &snapshot.entry;
        anyhow::ensure!(
            entry.is_current()
                && !entry.ids.is_empty()
                && entry.ids.windows(2).all(|pair| pair[0] < pair[1]),
            "invalid rename move journal entry"
        );
        for id in &entry.ids {
            super::validate_instance_id(id)?;
            anyhow::ensure!(
                checkpoint.session_ids.binary_search(id).is_ok(),
                "move journal owner is absent from rename checkpoint"
            );
        }
        super::validate_profile_name(&entry.source_profile)?;
        super::validate_profile_name(&entry.target_profile)?;
        anyhow::ensure!(
            entry.source_profile != entry.target_profile
                && (entry.source_profile == checkpoint.old
                    || entry.target_profile == checkpoint.old)
                && entry.source_profile != checkpoint.new
                && entry.target_profile != checkpoint.new
                && snapshot.directory_profile == entry.source_profile,
            "invalid rename move journal endpoints"
        );
        anyhow::ensure!(
            entry.source_sessions_path
                == super::get_profile_dir_path(&entry.source_profile)?.join("sessions.json")
                && entry.target_sessions_path
                    == super::get_profile_dir_path(&entry.target_profile)?.join("sessions.json"),
            "invalid rename move journal paths"
        );
    }
    anyhow::ensure!(
        checkpoint.move_journals.windows(2).all(|pair| {
            (&pair[0].directory_profile, &pair[0].file_name)
                < (&pair[1].directory_profile, &pair[1].file_name)
        }),
        "unordered rename move journals"
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
    if let Some(checkpoint) = read_delete_checkpoint()? {
        anyhow::ensure!(
            profile != checkpoint.profile,
            "profile deletion remains pending: {}",
            checkpoint.profile
        );
    }
    Ok(())
}

pub(crate) fn ensure_lifecycle_recovery_available(profile: &str, session_id: &str) -> Result<()> {
    let rename = read_checkpoint()?;
    let deletion = read_delete_checkpoint()?;
    if let Some(checkpoint) = rename {
        anyhow::ensure!(
            profile != checkpoint.old
                && profile != checkpoint.new
                && checkpoint
                    .session_ids
                    .binary_search_by(|id| id.as_str().cmp(session_id))
                    .is_err(),
            "profile rename retains lifecycle authority for session {session_id}"
        );
    }
    if let Some(checkpoint) = deletion {
        anyhow::ensure!(
            profile != checkpoint.profile,
            "profile deletion remains pending: {}",
            checkpoint.profile
        );
    }
    Ok(())
}

pub(crate) fn ensure_move_available(entry: &super::move_journal::MoveJournalEntry) -> Result<()> {
    ensure_profile_available(&entry.source_profile)?;
    ensure_profile_available(&entry.target_profile)?;
    for id in &entry.ids {
        ensure_lifecycle_recovery_available(&entry.source_profile, id)?;
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

fn retargeted_move_entry(
    checkpoint: &RenameCheckpoint,
    snapshot: &MoveJournalSnapshot,
    target: &str,
) -> Result<super::move_journal::MoveJournalEntry> {
    let mut entry = snapshot.entry.clone();
    let sessions = super::get_profile_dir_path(target)?.join("sessions.json");
    if entry.source_profile == checkpoint.old || entry.source_profile == checkpoint.new {
        entry.source_profile = target.to_owned();
        entry.source_sessions_path = sessions.clone();
    }
    if entry.target_profile == checkpoint.old || entry.target_profile == checkpoint.new {
        entry.target_profile = target.to_owned();
        entry.target_sessions_path = sessions;
    }
    anyhow::ensure!(
        entry.source_profile != entry.target_profile,
        "profile rename would collapse a move journal"
    );
    Ok(entry)
}

fn move_journal_path(
    checkpoint: &RenameCheckpoint,
    snapshot: &MoveJournalSnapshot,
    physical_profile: &str,
) -> Result<PathBuf> {
    let directory = if snapshot.directory_profile == checkpoint.old {
        physical_profile
    } else {
        &snapshot.directory_profile
    };
    Ok(super::get_profile_dir_path(directory)?
        .join(".move-journal")
        .join(&snapshot.file_name))
}

fn validate_move_journal_inventory(
    checkpoint: &RenameCheckpoint,
    physical_profile: &str,
) -> Result<()> {
    let profiles = super::list_profiles()?;
    let sessions_paths = profiles
        .iter()
        .map(|profile| super::get_profile_dir_path(profile).map(|path| path.join("sessions.json")))
        .collect::<Result<Vec<_>>>()?;
    let scan = super::move_journal::scan(sessions_paths);
    anyhow::ensure!(
        scan.unreadable_dirs.is_empty(),
        "move journals cannot be enumerated"
    );
    let mut seen = vec![false; checkpoint.move_journals.len()];
    for (path, parsed) in scan.entries {
        let directory_profile = profiles
            .iter()
            .find(|profile| {
                super::get_profile_dir_path(profile)
                    .is_ok_and(|dir| path.parent() == Some(dir.join(".move-journal").as_path()))
            })
            .context("move journal directory is not a profile directory")?;
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("move journal name is not UTF-8")?;
        anyhow::ensure!(
            file_name.ends_with(".json")
                && super::move_journal::file_created_at_nanos(&path).is_some(),
            "move journal name is not canonical"
        );
        let snapshot = checkpoint
            .move_journals
            .iter()
            .enumerate()
            .find(|(_, snapshot)| {
                let expected_directory = if snapshot.directory_profile == checkpoint.old {
                    physical_profile
                } else {
                    &snapshot.directory_profile
                };
                directory_profile == expected_directory && snapshot.file_name == file_name
            });
        let entry = match parsed {
            Ok(entry) => entry,
            Err(error) => {
                let (index, snapshot) = snapshot
                    .with_context(|| format!("untracked malformed move journal: {error}"))?;
                anyhow::ensure!(!seen[index], "duplicate rename move journal");
                anyhow::ensure!(
                    super::move_journal::file_identity(&path)? == snapshot.identity,
                    "move journal identity changed during profile rename"
                );
                seen[index] = true;
                continue;
            }
        };
        anyhow::ensure!(
            entry.is_current()
                && !entry.ids.is_empty()
                && entry.ids.windows(2).all(|pair| pair[0] < pair[1]),
            "invalid move journal entry during profile rename"
        );
        for id in &entry.ids {
            super::validate_instance_id(id)?;
        }
        super::validate_profile_name(&entry.source_profile)?;
        super::validate_profile_name(&entry.target_profile)?;
        anyhow::ensure!(
            entry.source_profile != entry.target_profile,
            "move journal endpoints are identical"
        );
        if let Some((index, snapshot)) = snapshot {
            anyhow::ensure!(!seen[index], "duplicate rename move journal");
            anyhow::ensure!(
                super::move_journal::file_identity(&path)? == snapshot.identity,
                "move journal identity changed during profile rename"
            );
            let old = retargeted_move_entry(checkpoint, snapshot, &checkpoint.old)?;
            let new = retargeted_move_entry(checkpoint, snapshot, &checkpoint.new)?;
            anyhow::ensure!(
                entry == old || entry == new,
                "move journal changed during profile rename"
            );
            seen[index] = true;
        } else {
            anyhow::ensure!(
                entry.source_profile != checkpoint.old
                    && entry.source_profile != checkpoint.new
                    && entry.target_profile != checkpoint.old
                    && entry.target_profile != checkpoint.new,
                "untracked move journal references a profile rename"
            );
            anyhow::ensure!(
                directory_profile == &entry.source_profile
                    && entry.source_sessions_path
                        == super::get_profile_dir_path(&entry.source_profile)?
                            .join("sessions.json")
                    && entry.target_sessions_path
                        == super::get_profile_dir_path(&entry.target_profile)?
                            .join("sessions.json"),
                "untracked move journal is not canonical"
            );
        }
    }
    anyhow::ensure!(
        seen.into_iter().all(|found| found),
        "rename move journal disappeared"
    );
    Ok(())
}
fn retarget_move_journals(checkpoint: &RenameCheckpoint, target: &str) -> Result<()> {
    let old_dir = super::get_profile_dir_path(&checkpoint.old)?;
    let new_dir = super::get_profile_dir_path(&checkpoint.new)?;
    let physical_profile = match (
        DirectoryIdentity::read(&old_dir)?,
        DirectoryIdentity::read(&new_dir)?,
    ) {
        (Some(identity), None) if identity == checkpoint.directory => checkpoint.old.as_str(),
        (None, Some(identity)) if identity == checkpoint.directory => checkpoint.new.as_str(),
        _ => anyhow::bail!("profile rename directory identity is ambiguous"),
    };
    validate_move_journal_inventory(checkpoint, physical_profile)?;
    for snapshot in &checkpoint.move_journals {
        let path = move_journal_path(checkpoint, snapshot, physical_profile)?;
        let bytes =
            fs::read(&path).with_context(|| format!("reading move journal {}", path.display()))?;
        let current = serde_json::from_slice::<super::move_journal::MoveJournalEntry>(&bytes).ok();
        let old = retargeted_move_entry(checkpoint, snapshot, &checkpoint.old)?;
        let new = retargeted_move_entry(checkpoint, snapshot, &checkpoint.new)?;
        anyhow::ensure!(
            current
                .as_ref()
                .is_none_or(|entry| entry == &old || entry == &new),
            "move journal changed during profile rename"
        );
        let desired = if target == checkpoint.old { old } else { new };
        if current.as_ref() != Some(&desired) {
            super::move_journal::update(&path, &snapshot.identity, &desired)?;
        } else {
            super::move_journal::sync(&path, &snapshot.identity)?;
        }
    }
    Ok(())
}

fn snapshot_move_journals(old: &str, new: &str) -> Result<Vec<MoveJournalSnapshot>> {
    let profiles = super::list_profiles()?;
    let sessions_paths = profiles
        .iter()
        .map(|profile| super::get_profile_dir_path(profile).map(|path| path.join("sessions.json")))
        .collect::<Result<Vec<_>>>()?;
    let scan = super::move_journal::scan(sessions_paths);
    anyhow::ensure!(
        scan.unreadable_dirs.is_empty(),
        "move journals cannot be enumerated"
    );
    let mut snapshots = Vec::new();
    for (path, entry) in scan.entries {
        let entry = entry.map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            !entry.ids.is_empty() && entry.ids.windows(2).all(|pair| pair[0] < pair[1]),
            "move journal ids are not sorted and unique"
        );
        for id in &entry.ids {
            super::validate_instance_id(id)?;
        }
        super::validate_profile_name(&entry.source_profile)?;
        super::validate_profile_name(&entry.target_profile)?;
        anyhow::ensure!(
            profiles.contains(&entry.source_profile) && profiles.contains(&entry.target_profile),
            "move journal references an absent profile"
        );
        anyhow::ensure!(
            entry.source_sessions_path
                == super::get_profile_dir_path(&entry.source_profile)?.join("sessions.json")
                && entry.target_sessions_path
                    == super::get_profile_dir_path(&entry.target_profile)?.join("sessions.json"),
            "move journal sessions path is not canonical"
        );
        anyhow::ensure!(
            entry.source_profile != entry.target_profile,
            "move journal endpoints are identical"
        );
        let directory_profile = profiles
            .iter()
            .find(|profile| {
                super::get_profile_dir_path(profile)
                    .is_ok_and(|dir| path.parent() == Some(dir.join(".move-journal").as_path()))
            })
            .context("move journal directory is not a profile directory")?;
        anyhow::ensure!(
            directory_profile == &entry.source_profile,
            "move journal is outside its source profile"
        );
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("move journal name is not UTF-8")?;
        anyhow::ensure!(
            super::move_journal::file_created_at_nanos(&path).is_some()
                && file_name.ends_with(".json"),
            "move journal name is not canonical"
        );
        let affected = entry.source_profile == old || entry.target_profile == old;
        anyhow::ensure!(
            entry.source_profile != new && entry.target_profile != new,
            "move journal references the unavailable rename target"
        );
        if affected {
            anyhow::ensure!(
                !(entry.source_profile == old && entry.target_profile == old),
                "move journal has duplicate renamed endpoints"
            );
            snapshots.push(MoveJournalSnapshot {
                directory_profile: directory_profile.clone(),
                file_name: file_name.to_owned(),
                identity: super::move_journal::file_identity(&path)?,
                entry,
            });
        }
    }
    snapshots.sort_by(|left, right| {
        (&left.directory_profile, &left.file_name)
            .cmp(&(&right.directory_profile, &right.file_name))
    });
    Ok(snapshots)
}

fn retarget(storage: &Storage, checkpoint: &RenameCheckpoint, target: &str) -> Result<()> {
    retarget_move_journals(checkpoint, target)?;
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
        sync_parent_directory(&path)?;
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

fn write_delete_checkpoint(checkpoint: &DeleteCheckpoint) -> Result<()> {
    let path = delete_checkpoint_path()?;
    atomic_write_verified(&path, &serde_json::to_vec(checkpoint)?)?;
    sync_parent_directory(&path)
}

fn deletion_inventory(
    storage: &Storage,
    profile: &str,
) -> Result<(Vec<StorageFlock>, Vec<std::fs::File>)> {
    let mut ids: Vec<String> = storage.load()?.into_iter().map(|row| row.id).collect();
    let journals = entries(storage)?;
    let logical_sessions_path = super::get_profile_dir_path(profile)?.join("sessions.json");
    for (_, entry) in &journals {
        anyhow::ensure!(
            entry.source_profile == profile && entry.sessions_path == logical_sessions_path,
            "foreign profile journal"
        );
        ids.push(entry.session_id.clone());
    }
    ids.extend(crate::process::worker_registry::profile_reference_sessions(
        profile,
    )?);

    let sessions_paths = super::list_profiles()?
        .into_iter()
        .map(|name| super::get_profile_dir_path(&name).map(|path| path.join("sessions.json")))
        .collect::<Result<Vec<_>>>()?;
    let moves = super::move_journal::scan(sessions_paths);
    anyhow::ensure!(
        moves.unreadable_dirs.is_empty(),
        "move journals cannot be enumerated"
    );
    for (_, entry) in moves.entries {
        let entry = entry.map_err(anyhow::Error::msg)?;
        for id in &entry.ids {
            super::validate_instance_id(id)?;
        }
        anyhow::ensure!(
            entry.source_profile != profile && entry.target_profile != profile,
            "profile has a pending session move"
        );
    }

    ids.sort();
    ids.dedup();
    let owners = owner_guards(storage, &ids)?;
    let registry =
        crate::process::worker_registry::acquire_profile_deletion_registry_guards(profile, &ids)?;
    let mut consumable = Vec::new();
    for row in storage.load()? {
        if let Some(runtime) = row.prime_runtime() {
            runtime.cleanup()?;
        }
    }
    for (path, entry) in &journals {
        if let Some(runtime) = entry.instance.prime_runtime() {
            runtime.cleanup()?;
        }
        match entry.phase {
            LifecyclePhase::Abandoned => consumable.push(path),
            LifecyclePhase::RowRemoved | LifecyclePhase::Kept
                if entry.kept_resources.is_empty()
                    || super::deletion::kept_resources_are_reclaimed(entry) =>
            {
                consumable.push(path);
            }
            LifecyclePhase::RowRemoved | LifecyclePhase::Kept => {
                anyhow::bail!("profile has retained session resources");
            }
            _ => anyhow::bail!("profile has a pending session lifecycle operation"),
        }
    }
    for path in consumable {
        lifecycle_journal::consume(path)?;
    }
    Ok((owners, registry))
}

fn finish_delete(checkpoint: &mut DeleteCheckpoint) -> Result<()> {
    let old_dir = super::get_profile_dir_path(&checkpoint.profile)?;
    let old = DirectoryIdentity::read(&old_dir)?;
    let tombstone = DirectoryIdentity::read(&checkpoint.tombstone)?;
    match checkpoint.phase {
        DeletePhase::Prepared => match (old.as_ref(), tombstone.as_ref()) {
            (Some(identity), None) if identity == &checkpoint.directory => {
                anyhow::ensure!(
                    super::get_profile_dir_path(&checkpoint.survivor)?.is_dir(),
                    "profile delete survivor disappeared"
                );
                if checkpoint.default_was_deleted {
                    super::update_config(|config| {
                        if config.default_profile == checkpoint.profile {
                            config.default_profile.clone_from(&checkpoint.survivor);
                        }
                    })?;
                }
                fs::rename(&old_dir, &checkpoint.tombstone)?;
                sync_parent_directory(&old_dir)?;
                sync_parent_directory(&checkpoint.tombstone)?;
                #[cfg(test)]
                super::storage::test_crash_point("profile-delete-after-rename");
                checkpoint.phase = DeletePhase::Committed;
                write_delete_checkpoint(checkpoint)?;
                #[cfg(test)]
                super::storage::test_crash_point("profile-delete-after-commit");
            }
            (None, Some(identity)) if identity == &checkpoint.directory => {
                sync_parent_directory(&old_dir)?;
                sync_parent_directory(&checkpoint.tombstone)?;
                checkpoint.phase = DeletePhase::Committed;
                write_delete_checkpoint(checkpoint)?;
            }
            _ => anyhow::bail!("profile delete directory identity is ambiguous"),
        },
        DeletePhase::Committed => {
            anyhow::ensure!(
                old.is_none()
                    && tombstone
                        .as_ref()
                        .is_none_or(|identity| identity == &checkpoint.directory),
                "committed profile delete directory identity is ambiguous"
            );
        }
    }
    if checkpoint.tombstone.try_exists()? {
        fs::remove_dir_all(&checkpoint.tombstone)?;
        sync_parent_directory(&checkpoint.tombstone)?;
    }
    lifecycle_journal::consume(&delete_checkpoint_path()?)
}

fn settle_delete(checkpoint: &mut DeleteCheckpoint) -> Result<()> {
    let old_dir = super::get_profile_dir_path(&checkpoint.profile)?;
    let old = DirectoryIdentity::read(&old_dir)?;
    let tombstone = DirectoryIdentity::read(&checkpoint.tombstone)?;
    let existing = match (old.as_ref(), tombstone.as_ref()) {
        (Some(identity), None) if identity == &checkpoint.directory => Some(old_dir),
        (None, Some(identity)) if identity == &checkpoint.directory => {
            Some(checkpoint.tombstone.clone())
        }
        (None, None) if checkpoint.phase == DeletePhase::Committed => None,
        _ => anyhow::bail!("profile delete directory identity is ambiguous"),
    };
    let profile = checkpoint.profile.clone();
    let storage = existing
        .as_deref()
        .map(|directory| Storage::open_profile_delete_recovery(&profile, directory))
        .transpose()?;
    Storage::with_profile_delete_recovery_locked(&profile, existing.as_deref(), || {
        let _owners = storage
            .as_ref()
            .map(|storage| deletion_inventory(storage, &profile))
            .transpose()?;
        finish_delete(checkpoint)
    })
}

pub(crate) fn delete(profile: &str) -> Result<()> {
    let _identity = super::storage::acquire_session_identity_lock()?;
    recover_pending()?;
    let storage = Storage::open_unwatched(profile)?;
    storage.with_profile_rename_locked(|| {
        let survivor = super::list_profiles()?
            .into_iter()
            .find(|candidate| candidate != profile)
            .with_context(|| {
                format!("Cannot delete '{profile}': at least one profile must exist")
            })?;
        let old_dir = super::get_profile_dir_path(profile)?;
        let directory = DirectoryIdentity::read(&old_dir)?
            .with_context(|| format!("Profile '{profile}' does not exist"))?;
        let _owners = deletion_inventory(&storage, profile)?;
        let mut checkpoint = DeleteCheckpoint {
            version: 1,
            profile: profile.to_owned(),
            directory,
            tombstone: old_dir
                .parent()
                .context("profile parent is missing")?
                .join(format!(
                    "{}{}",
                    super::PROFILE_DELETE_TOMBSTONE_PREFIX,
                    uuid::Uuid::new_v4().simple()
                )),
            survivor,
            default_was_deleted: super::load_config()?
                .is_some_and(|config| config.default_profile == profile),
            phase: DeletePhase::Prepared,
        };
        write_delete_checkpoint(&checkpoint)?;
        #[cfg(test)]
        super::storage::test_crash_point("profile-delete-after-checkpoint");
        finish_delete(&mut checkpoint)
    })
}

pub(crate) fn recover_pending() -> Result<()> {
    if let Some(checkpoint) = read_checkpoint()? {
        settle(&checkpoint)?;
    }
    if let Some(mut checkpoint) = read_delete_checkpoint()? {
        settle_delete(&mut checkpoint)?;
    }
    Ok(())
}

/// Caller holds the session identity lock.
pub(crate) fn recover_pending_scoped() -> Result<()> {
    match recover_pending() {
        Ok(()) => Ok(()),
        Err(error) => {
            let rename = read_checkpoint()?;
            let deletion = read_delete_checkpoint()?;
            if rename.is_none() && deletion.is_none() {
                return Err(error);
            }
            tracing::warn!(
                target: "session.delete_recovery",
                %error,
                "profile transition remains fenced; allowing independent profiles"
            );
            Ok(())
        }
    }
}

pub(crate) fn recover_once() -> Result<()> {
    if !checkpoint_path()?.try_exists()? && !delete_checkpoint_path()?.try_exists()? {
        return Ok(());
    }
    let Some(_identity) = super::storage::try_acquire_session_identity_lock()? else {
        return Ok(());
    };
    recover_pending_scoped()
}

pub(crate) fn rename(old: &str, new: &str) -> Result<()> {
    let _identity = super::storage::acquire_session_identity_lock()?;
    recover_pending()?;
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
        let move_journals = snapshot_move_journals(old, new)?;
        ids.extend(
            move_journals
                .iter()
                .flat_map(|snapshot| snapshot.entry.ids.iter().cloned()),
        );
        ids.sort();
        ids.dedup();
        let _guards = owner_guards(&storage, &ids)?;
        let checkpoint = RenameCheckpoint {
            version: 1,
            old: old.to_owned(),
            new: new.to_owned(),
            directory,
            session_ids: ids,
            move_journals,
            default_was_old: super::load_config()?
                .is_some_and(|config| config.default_profile == old),
        };
        let path = checkpoint_path()?;
        atomic_write_verified(&path, &serde_json::to_vec(&checkpoint)?)?;
        sync_parent_directory(&path)?;
        #[cfg(test)]
        super::storage::test_crash_point("profile-rename-after-checkpoint");
        retarget(&storage, &checkpoint, new)?;
        crate::process::rename_exclusive_path(&old_dir, &new_dir)?;
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
        let target = super::super::get_profile_dir_path("after").unwrap();
        fs::create_dir_all(&target).unwrap();
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

    #[tokio::test]
    #[serial]
    async fn pending_rename_scopes_recovery_without_discarding_unknown_authority() {
        use clap::Parser;

        for case in ["independent", "shared-owner", "bad-rename", "bad-delete"] {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let purge = |profile: &str, id: &str| {
                let storage = Storage::new_unwatched(profile).unwrap();
                let workspace =
                    crate::session::scratch::provision_scratch_dir(&format!("{profile}-{id}"))
                        .unwrap();
                fs::write(workspace.join("retained"), b"pending purge").unwrap();
                let mut row = Instance::new(id, workspace.to_str().unwrap());
                row.id = id.into();
                row.source_profile = profile.into();
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
                let journal = lifecycle_journal::record(
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
                    .with_phase(LifecyclePhase::TeardownStarted),
                )
                .unwrap();
                (storage, workspace, journal)
            };
            let (_, affected_workspace, affected_journal) = purge("before", "rename-owner");
            let witness_id = if case == "shared-owner" {
                "rename-owner"
            } else {
                "independent-owner"
            };
            let (witness, witness_workspace, witness_journal) = purge("witness", witness_id);
            let interruption = {
                let _point =
                    super::super::storage::ArmedCrashPoint::arm("profile-rename-after-directory");
                std::panic::catch_unwind(|| super::super::rename_profile("before", "after"))
            };
            assert!(interruption.is_err());
            let foreign = super::super::get_profile_dir_path("before").unwrap();
            fs::create_dir_all(&foreign).unwrap();
            fs::write(foreign.join("foreign"), b"do not delete").unwrap();
            if case == "bad-rename" {
                fs::write(checkpoint_path().unwrap(), b"{").unwrap();
            }
            if case == "bad-delete" {
                fs::write(delete_checkpoint_path().unwrap(), b"{").unwrap();
            }
            let affected = Storage::open_unwatched("after").unwrap();
            let affected_journal = affected
                .sessions_path()
                .parent()
                .unwrap()
                .join(".lifecycle-journal")
                .join(affected_journal.file_name().unwrap());
            let affected_rows_before = fs::read(affected.sessions_path()).unwrap();
            let affected_journal_before = fs::read(&affected_journal).unwrap();
            let checkpoint_before = fs::read(checkpoint_path().unwrap()).unwrap();
            let witness_before = fs::read(witness.sessions_path()).unwrap();
            let journal_before = fs::read(&witness_journal).unwrap();

            let cli = crate::cli::Cli::try_parse_from([
                "aoe",
                "-p",
                "witness",
                "group",
                "create",
                "admission-probe",
            ])
            .unwrap();
            let Some(crate::cli::Commands::Group { command }) = cli.command else {
                unreachable!();
            };
            let admission = crate::cli::group::run(cli.profile.as_deref().unwrap(), command).await;
            assert_eq!(
                admission.is_err(),
                case.starts_with("bad-"),
                "{case}: {admission:?}"
            );
            let (_, groups) = witness.load_with_groups().unwrap();
            assert_eq!(
                groups.iter().any(|group| group.path == "admission-probe"),
                !case.starts_with("bad-"),
                "{case}"
            );
            assert_eq!(
                super::super::admit_tui_profiles("").is_err(),
                case.starts_with("bad-"),
                "{case}"
            );
            for endpoint in ["before", "after"] {
                assert!(super::super::admit_profile(endpoint).is_err(), "{case}");
                assert!(
                    super::super::admit_tui_profiles(endpoint).is_err(),
                    "{case}"
                );
            }

            let result = super::super::deletion::recover_lifecycle_journals_once();

            if case == "independent" {
                result.unwrap();
                assert!(!witness_workspace.exists());
                assert!(!witness_journal.exists());
                assert!(witness.load().unwrap().is_empty());
            } else {
                assert_eq!(result.is_err(), case.starts_with("bad-"), "{case}");
                assert!(witness_workspace.join("retained").exists(), "{case}");
                assert_eq!(fs::read(witness.sessions_path()).unwrap(), witness_before);
                assert_eq!(fs::read(witness_journal).unwrap(), journal_before);
            }
            assert!(affected_workspace.join("retained").exists(), "{case}");
            assert_eq!(
                fs::read(affected.sessions_path()).unwrap(),
                affected_rows_before
            );
            assert_eq!(fs::read(affected_journal).unwrap(), affected_journal_before);
            if case == "bad-delete" {
                assert_eq!(fs::read(delete_checkpoint_path().unwrap()).unwrap(), b"{");
            }
            assert_eq!(
                fs::read(checkpoint_path().unwrap()).unwrap(),
                checkpoint_before
            );
            assert_eq!(fs::read(foreign.join("foreign")).unwrap(), b"do not delete");
            if case == "independent" {
                let preserved = temp.path().join("foreign-profile");
                fs::rename(&foreign, &preserved).unwrap();
                super::super::deletion::recover_lifecycle_journals_once().unwrap();
                assert!(!checkpoint_path().unwrap().exists());
                assert!(!affected_workspace.exists());
                assert_eq!(
                    fs::read(preserved.join("foreign")).unwrap(),
                    b"do not delete"
                );
            }
        }
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
    fn interrupted_rename_settles_before_purge_and_cannot_recreate_its_profile() {
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
                }
                assert!(storage
                    .update(|rows, _| {
                        rows.clear();
                        Ok(())
                    })
                    .is_err());
                let pending_target = Storage::open_unwatched(original).unwrap();
                let checkpoint_before = fs::read(checkpoint_path().unwrap()).unwrap();
                assert!(crate::session::validate_profile_admission(original).is_err());
                assert_eq!(
                    fs::read(checkpoint_path().unwrap()).unwrap(),
                    checkpoint_before
                );
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
                assert_eq!(migrated.runner_identity, record.runner_identity);
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

    #[test]
    #[serial]
    fn profile_delete_refuses_live_owner_then_removes_an_ordinary_nonempty_profile() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::create_profile("keeper").unwrap();
        crate::session::create_profile("victim").unwrap();
        let storage = Storage::open_unwatched("victim").unwrap();
        let mut row = Instance::new("owned", "/tmp/owned");
        row.id = "owned".into();
        row.source_profile = "victim".into();
        storage
            .update(|rows, _| {
                rows.push(row);
                Ok(())
            })
            .unwrap();
        let owner = storage
            .try_acquire_purge_owner_lock("owned")
            .unwrap()
            .unwrap();

        assert!(crate::session::delete_profile("victim").is_err());
        assert!(crate::session::get_profile_dir_path("victim")
            .unwrap()
            .is_dir());
        drop(owner);
        let stopped = crate::process::worker_registry::WorkerRecord::new(
            "owned".into(),
            std::process::id(),
            crate::process::worker_registry::socket_path_for("owned").unwrap(),
            "agent".into(),
            "agent".into(),
            "/tmp/owned".into(),
            None,
            vec![],
            vec![],
            None,
            Some("victim".into()),
        );
        crate::process::worker_registry::save(&stopped).unwrap();
        crate::process::worker_registry::request_stop(&stopped).unwrap();
        crate::process::worker_registry::retire_selected(&stopped).unwrap();
        assert!(crate::process::worker_registry::stop_requested("owned"));
        crate::process::worker_registry::fence_for_purge("rowless", "victim", 1).unwrap();
        assert!(crate::session::delete_profile("victim").is_err());
        assert!(crate::session::get_profile_dir_path("victim")
            .unwrap()
            .is_dir());
        assert!(crate::process::worker_registry::clear_purge_fence_if_owned(
            "rowless", "victim", 1,
        )
        .unwrap());
        let corrupt = crate::process::worker_registry::workers_dir()
            .unwrap()
            .join("corrupt.purge-fence");
        fs::write(&corrupt, b"{").unwrap();
        assert!(crate::session::delete_profile("victim").is_err());
        assert!(crate::session::get_profile_dir_path("victim")
            .unwrap()
            .is_dir());
        fs::remove_file(corrupt).unwrap();
        crate::session::delete_profile("victim").unwrap();
        assert!(!crate::session::get_profile_dir_path("victim")
            .unwrap()
            .exists());
    }

    #[test]
    #[serial]
    fn profile_delete_requires_kept_resources_to_be_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::create_profile("keeper").unwrap();
        crate::session::create_profile("victim").unwrap();
        let storage = Storage::open_unwatched("victim").unwrap();
        let kept = temp.path().join("kept");
        fs::create_dir(&kept).unwrap();
        let mut row = Instance::new("kept", kept.to_str().unwrap());
        row.id = "kept".into();
        row.source_profile = "victim".into();
        row.scratch = true;
        let mut entry = LifecycleJournalEntry::deletion(
            row,
            Status::Deleting,
            storage.sessions_path().to_path_buf(),
            lifecycle_journal::LifecycleDeletionOptions {
                generation: 1,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: true,
                purge_acp_transcript: false,
            },
        )
        .with_phase(LifecyclePhase::Kept);
        entry.kept_resources = vec![format!("Scratch directory kept at: {}", kept.display())];
        lifecycle_journal::record(&entry).unwrap();

        assert!(crate::session::delete_profile("victim").is_err());
        assert!(crate::session::get_profile_dir_path("victim")
            .unwrap()
            .is_dir());
        fs::remove_dir(&kept).unwrap();
        crate::session::delete_profile("victim").unwrap();
        assert!(!crate::session::get_profile_dir_path("victim")
            .unwrap()
            .exists());
    }

    #[test]
    #[serial]
    fn profile_delete_validates_all_journals_before_consuming_terminal_authority() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        crate::session::create_profile("keeper").unwrap();
        crate::session::create_profile("victim").unwrap();
        let storage = Storage::open_unwatched("victim").unwrap();
        let kept = temp.path().join("kept-terminal");
        fs::create_dir(&kept).unwrap();
        let mut row = Instance::new("terminal", kept.to_str().unwrap());
        row.id = "terminal".into();
        row.source_profile = "victim".into();
        row.scratch = true;
        let terminal = LifecycleJournalEntry::deletion(
            row,
            Status::Deleting,
            storage.sessions_path().to_path_buf(),
            lifecycle_journal::LifecycleDeletionOptions {
                generation: 10,
                delete_worktree: false,
                delete_branch: false,
                delete_sandbox: false,
                force_delete: false,
                detach_hooks: true,
                keep_scratch: true,
                purge_acp_transcript: false,
            },
        )
        .with_phase(LifecyclePhase::RowRemoved)
        .with_kept_resources(vec![format!(
            "Scratch directory kept at: {}",
            kept.display()
        )]);
        let terminal_path = lifecycle_journal::record(&terminal).unwrap();
        let mut older = terminal
            .with_phase(LifecyclePhase::TeardownStarted)
            .with_kept_resources(Vec::new());
        older.generation = 9;
        older.instance.lifecycle_generation = 9;
        if let Some(reservation) = older.instance.lifecycle_reservation.as_mut() {
            reservation.generation = 9;
        }
        older.keep_scratch = false;
        let older_path = lifecycle_journal::record(&older).unwrap();

        assert!(crate::session::delete_profile("victim").is_err());
        assert!(terminal_path.exists());
        assert!(older_path.exists());
        assert!(kept.exists());
    }

    #[test]
    #[serial]
    fn interrupted_profile_delete_recovers_at_every_durable_boundary() {
        for point in [
            "profile-delete-after-checkpoint",
            "profile-delete-after-rename",
            "profile-delete-after-commit",
        ] {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            crate::session::create_profile("keeper").unwrap();
            crate::session::create_profile("victim").unwrap();
            crate::session::set_default_profile("victim").unwrap();
            Storage::open_unwatched("victim")
                .unwrap()
                .update(|_, _| Ok(()))
                .unwrap();

            let crashed = {
                let _crash = super::super::storage::ArmedCrashPoint::arm(point);
                std::panic::catch_unwind(|| crate::session::delete_profile("victim"))
            };
            assert!(crashed.is_err(), "{point}");
            let checkpoint = read_delete_checkpoint().unwrap().unwrap();
            assert_eq!(
                checkpoint.tombstone.parent(),
                crate::session::get_profile_dir_path("victim")
                    .unwrap()
                    .parent()
            );
            assert!(crate::session::list_profiles().unwrap().iter().all(
                |profile| !profile.starts_with(crate::session::PROFILE_DELETE_TOMBSTONE_PREFIX)
            ));
            crate::process::worker_registry::fence_for_purge("late-rowless", "victim", 99).unwrap();
            assert!(
                recover_pending().is_err(),
                "{point}: recovery ignored new global lifecycle evidence"
            );
            assert!(crate::process::worker_registry::clear_purge_fence_if_owned(
                "late-rowless",
                "victim",
                99,
            )
            .unwrap());
            recover_pending().unwrap();

            assert!(
                !crate::session::get_profile_dir_path("victim")
                    .unwrap()
                    .exists(),
                "{point}"
            );
            assert!(crate::session::get_profile_dir_path("keeper")
                .unwrap()
                .is_dir());
            assert_eq!(
                crate::session::load_config()
                    .unwrap()
                    .unwrap()
                    .default_profile,
                "keeper"
            );
            assert!(!delete_checkpoint_path().unwrap().exists());
            assert!(
                fs::read_dir(crate::session::get_app_dir().unwrap().join("profiles"),)
                    .unwrap()
                    .flatten()
                    .all(|entry| !entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(crate::session::PROFILE_DELETE_TOMBSTONE_PREFIX)),
                "{point}"
            );
            recover_pending().unwrap();
        }
    }
    #[test]
    #[serial]
    fn rename_retargets_interrupted_move_journals() {
        for renamed in ["source", "target"] {
            for crash in [
                None,
                Some("profile-rename-move-after-truncate"),
                Some("profile-rename-move-after-write"),
            ] {
                let temp = tempfile::tempdir().unwrap();
                let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
                let source = Storage::new_unwatched("source").unwrap();
                let target = Storage::new_unwatched("target").unwrap();
                let mut row = Instance::new("move", temp.path().to_str().unwrap());
                row.id = format!("move-{renamed}-{}", crash.unwrap_or("complete"));
                row.source_profile = "source".into();
                source
                    .update(|rows, _| {
                        rows.push(row.clone());
                        Ok(())
                    })
                    .unwrap();
                target.update(|_, _| Ok(())).unwrap();

                {
                    let _crash = super::super::storage::ArmedCrashPoint::arm("profile-move-target");
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        source
                            .move_instance_to_with_effect(
                                &target,
                                &row,
                                &row,
                                false,
                                |_, _| Ok(()),
                                |_| Ok(()),
                            )
                            .unwrap();
                    }));
                }
                assert_eq!(source.load().unwrap().len(), 1);
                assert_eq!(target.load().unwrap().len(), 1);

                let renamed_profile = format!("{renamed}-renamed");
                if let Some(point) = crash {
                    let crashed = {
                        let _crash = super::super::storage::ArmedCrashPoint::arm(point);
                        std::panic::catch_unwind(|| {
                            crate::session::rename_profile(renamed, &renamed_profile)
                        })
                    };
                    assert!(crashed.is_err(), "{renamed}, {point}");
                    recover_pending().unwrap();
                    if !crate::session::get_profile_dir_path(&renamed_profile)
                        .unwrap()
                        .exists()
                    {
                        crate::session::rename_profile(renamed, &renamed_profile).unwrap();
                    }
                } else {
                    crate::session::rename_profile(renamed, &renamed_profile).unwrap();
                }
                let source_name = if renamed == "source" {
                    renamed_profile.as_str()
                } else {
                    "source"
                };
                let target_name = if renamed == "target" {
                    renamed_profile.as_str()
                } else {
                    "target"
                };
                let source = Storage::open_unwatched(source_name).unwrap();
                let target = Storage::open_unwatched(target_name).unwrap();
                let source_rows = source.load().unwrap();
                let target_rows = target.load().unwrap();
                let loaded = [
                    (source_name, source_rows.as_slice()),
                    (target_name, target_rows.as_slice()),
                ];
                let storages = [(source_name, &source), (target_name, &target)];
                let outcome =
                    super::super::storage::reconcile_profile_duplicates(&loaded, &storages);

                assert!(outcome.repaired, "{renamed}, {crash:?}");
                assert!(outcome.reports.is_empty(), "{renamed}, {crash:?}");
                assert!(source.load().unwrap().is_empty(), "{renamed}, {crash:?}");
                assert_eq!(target.load().unwrap().len(), 1, "{renamed}, {crash:?}");
                let scan = super::super::move_journal::scan([
                    source.sessions_path().to_path_buf(),
                    target.sessions_path().to_path_buf(),
                ]);
                assert!(scan.unreadable_dirs.is_empty(), "{renamed}, {crash:?}");
                assert!(scan.entries.is_empty(), "{renamed}, {crash:?}");
            }
        }
    }

    #[test]
    #[serial]
    fn rename_recovery_rejects_a_byte_identical_move_journal_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let source = Storage::new_unwatched("source").unwrap();
        let target = Storage::new_unwatched("target").unwrap();
        let entry = super::super::move_journal::MoveJournalEntry {
            version: super::super::move_journal::MOVE_JOURNAL_VERSION,
            ids: vec!["replacement".into()],
            source_profile: "source".into(),
            target_profile: "target".into(),
            source_sessions_path: source.sessions_path().to_path_buf(),
            target_sessions_path: target.sessions_path().to_path_buf(),
            group_move_source_path: String::new(),
            group_move_target_path: String::new(),
            group_move_subtree: false,
            created_at_epoch_ms: 1,
        };
        let journal = super::super::move_journal::record(&entry, source.sessions_path()).unwrap();
        let original = fs::read(&journal).unwrap();

        let crashed = {
            let _crash =
                super::super::storage::ArmedCrashPoint::arm("profile-rename-after-checkpoint");
            std::panic::catch_unwind(|| crate::session::rename_profile("source", "renamed"))
        };
        assert!(crashed.is_err());

        let replacement = journal.with_extension("replacement");
        fs::write(&replacement, &original).unwrap();
        fs::rename(&replacement, &journal).unwrap();
        let error = recover_pending().unwrap_err();
        assert!(error.to_string().contains("identity changed"), "{error:#}");
        assert!(checkpoint_path().unwrap().exists());
        assert!(crate::session::get_profile_dir_path("source")
            .unwrap()
            .is_dir());
        assert!(!crate::session::get_profile_dir_path("renamed")
            .unwrap()
            .exists());
    }
}
