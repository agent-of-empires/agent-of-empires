//! Profile-directory commit and durable lifecycle-reference settlement.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::process::DirectoryIdentity;

use super::lifecycle_journal::{self, LifecycleJournalEntry};
use super::storage::{atomic_write_verified, sync_parent_directory, StorageFlock};
use super::Storage;

const CHECKPOINT: &str = ".profile-rename.json";
const PENDING_CHECKPOINT_PREFIX: &str = ".profile-rename.json.pending-";
const CORRUPT_CHECKPOINT_MARKER: &str = ".corrupt-";

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

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn sidecar_checkpoint_path(old: &str, new: &str) -> Result<PathBuf> {
    let identity = sha256_hex(format!("{old}\0{new}").as_bytes());
    Ok(super::get_app_dir()?.join(format!("{PENDING_CHECKPOINT_PREFIX}{identity}.json")))
}

fn checkpoint_paths() -> Result<Vec<PathBuf>> {
    let app_dir = super::get_app_dir()?;
    let primary = app_dir.join(CHECKPOINT);
    let mut paths = Vec::new();
    match fs::symlink_metadata(&primary) {
        Ok(_) => paths.push(primary),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(&app_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let pending = name.starts_with(PENDING_CHECKPOINT_PREFIX) && name.ends_with(".json");
        let corrupt = name.starts_with(&format!("{CHECKPOINT}{CORRUPT_CHECKPOINT_MARKER}"))
            || (name.starts_with(PENDING_CHECKPOINT_PREFIX)
                && name.contains(CORRUPT_CHECKPOINT_MARKER));
        if pending || corrupt {
            paths.push(entry.path());
        }
    }
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn active_checkpoint_paths() -> Result<Vec<PathBuf>> {
    Ok(checkpoint_paths()?
        .into_iter()
        .filter(|path| !is_quarantined_checkpoint(path))
        .collect())
}

fn checkpoint_path_for_name(name: &str) -> Result<PathBuf> {
    let active = name == CHECKPOINT
        || name
            .strip_prefix(PENDING_CHECKPOINT_PREFIX)
            .and_then(|name| name.strip_suffix(".json"))
            .is_some_and(|identity| {
                identity.len() == 64
                    && identity
                        .chars()
                        .all(|character| character.is_ascii_hexdigit())
            });
    anyhow::ensure!(active, "not an active profile rename checkpoint filename");
    Ok(super::get_app_dir()?.join(name))
}

fn validate_checkpoint(checkpoint: &RenameCheckpoint) -> Result<()> {
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
    Ok(())
}

fn checkpoint_profiles(value: &serde_json::Value) -> Option<(String, String)> {
    let old = value.get("old")?.as_str()?.to_owned();
    let new = value.get("new")?.as_str()?.to_owned();
    if old == new
        || super::validate_profile_name(&old).is_err()
        || super::validate_new_profile_name(&new).is_err()
    {
        return None;
    }
    Some((old, new))
}

enum CheckpointState {
    Missing,
    Valid(RenameCheckpoint),
    Invalid {
        bytes: Vec<u8>,
        error: String,
        affected_profiles: Option<(String, String)>,
    },
}

type CheckpointRead = std::result::Result<CheckpointState, String>;
type CheckpointSet = Vec<(PathBuf, CheckpointRead)>;

fn read_checkpoint_at(path: &Path) -> Result<CheckpointState> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CheckpointState::Missing)
        }
        Err(error) => return Err(error.into()),
    };
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "profile rename checkpoint is not a regular file"
    );
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CheckpointState::Missing)
        }
        Err(error) => return Err(error.into()),
    };
    let parsed = serde_json::from_slice::<serde_json::Value>(&bytes);
    let (affected_profiles, error) = match parsed {
        Ok(value) => {
            let affected_profiles = checkpoint_profiles(&value);
            match serde_json::from_value::<RenameCheckpoint>(value) {
                Ok(checkpoint) => match validate_checkpoint(&checkpoint) {
                    Ok(()) => return Ok(CheckpointState::Valid(checkpoint)),
                    Err(error) => (affected_profiles, error.to_string()),
                },
                Err(error) => (affected_profiles, error.to_string()),
            }
        }
        Err(error) => (None, error.to_string()),
    };
    Ok(CheckpointState::Invalid {
        bytes,
        error,
        affected_profiles,
    })
}

fn quarantine_checkpoint(path: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let metadata = fs::symlink_metadata(path)?;
    anyhow::ensure!(
        metadata.file_type().is_file(),
        "profile rename checkpoint is not a regular file"
    );
    let digest = sha256_hex(bytes);
    let name = path
        .file_name()
        .context("profile rename checkpoint has no filename")?;
    let quarantine = path.with_file_name(format!("{}.corrupt-{digest}", name.to_string_lossy()));
    match fs::hard_link(path, &quarantine) {
        Ok(()) => sync_parent_directory(path)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(&quarantine)?;
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "profile rename quarantine path is not a regular file"
            );
            anyhow::ensure!(
                fs::read(&quarantine)? == bytes,
                "profile rename quarantine path contains different data"
            );
        }
        Err(error) => return Err(error.into()),
    }
    fs::File::open(&quarantine)?.sync_all()?;
    anyhow::ensure!(
        fs::read(path)? == bytes,
        "profile rename checkpoint changed while it was being quarantined"
    );
    Ok(quarantine)
}

fn is_quarantined_checkpoint(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(CORRUPT_CHECKPOINT_MARKER))
}

fn read_checkpoint_set() -> Result<CheckpointSet> {
    Ok(active_checkpoint_paths()?
        .into_iter()
        .map(|path| {
            let state = read_checkpoint_at(&path).map_err(|error| format!("{error:#}"));
            (path, state)
        })
        .collect())
}

fn validate_checkpoint_repair_scope(
    affected_profiles: Option<(String, String)>,
    expected_profiles: Option<(&str, &str)>,
) -> Result<()> {
    match (affected_profiles, expected_profiles) {
        (Some((old, new)), Some((expected_old, expected_new))) => {
            anyhow::ensure!(
                old == expected_old && new == expected_new,
                "checkpoint endpoints do not match the acknowledged profile names"
            );
        }
        (Some(_), None) => {
            anyhow::bail!(
                "checkpoint endpoints are identifiable; provide --old-profile and --new-profile"
            );
        }
        (None, Some(_)) => {
            anyhow::bail!("checkpoint endpoints are unknown; profile names cannot be verified");
        }
        (None, None) => {}
    }
    Ok(())
}

pub(crate) fn repair_rename_checkpoint(
    name: &str,
    expected_sha256: &str,
    expected_profiles: Option<(&str, &str)>,
) -> Result<PathBuf> {
    anyhow::ensure!(
        expected_sha256.len() == 64
            && expected_sha256
                .chars()
                .all(|character| character.is_ascii_hexdigit()),
        "checkpoint SHA-256 must contain 64 hexadecimal characters"
    );
    if let Some((old, new)) = expected_profiles {
        super::validate_profile_name(old)?;
        super::validate_new_profile_name(new)?;
        anyhow::ensure!(old != new, "invalid rename identity");
    }
    let _identity = super::storage::acquire_session_identity_lock()?;
    let path = checkpoint_path_for_name(name)?;
    let expected_sha256 = expected_sha256.to_ascii_lowercase();
    let quarantine = path.with_file_name(format!(
        "{name}{CORRUPT_CHECKPOINT_MARKER}{expected_sha256}"
    ));

    match fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let metadata = fs::symlink_metadata(&quarantine)
                .context("active checkpoint and preserved copy are missing")?;
            anyhow::ensure!(
                metadata.file_type().is_file(),
                "preserved checkpoint is not a regular file"
            );
            let bytes = fs::read(&quarantine)?;
            anyhow::ensure!(
                sha256_hex(&bytes) == expected_sha256,
                "preserved checkpoint does not match the supplied SHA-256"
            );
            let affected_profiles = match read_checkpoint_at(&quarantine)? {
                CheckpointState::Invalid {
                    affected_profiles, ..
                } => affected_profiles,
                _ => anyhow::bail!("preserved checkpoint is not invalid"),
            };
            validate_checkpoint_repair_scope(affected_profiles, expected_profiles)?;
            return Ok(quarantine);
        }
        Err(error) => return Err(error.into()),
        Ok(_) => {}
    }

    let (bytes, affected_profiles) = match read_checkpoint_at(&path)? {
        CheckpointState::Invalid {
            bytes,
            affected_profiles,
            ..
        } => (bytes, affected_profiles),
        CheckpointState::Valid(_) => {
            anyhow::bail!("checkpoint is valid and must be recovered normally")
        }
        CheckpointState::Missing => anyhow::bail!("active checkpoint is missing"),
    };
    anyhow::ensure!(
        sha256_hex(&bytes) == expected_sha256,
        "active checkpoint does not match the supplied SHA-256"
    );
    validate_checkpoint_repair_scope(affected_profiles, expected_profiles)?;

    let quarantine = quarantine_checkpoint(&path, &bytes)?;
    anyhow::ensure!(
        fs::read(&path)? == bytes,
        "active checkpoint changed before repair"
    );
    fs::remove_file(&path)?;
    sync_parent_directory(&path)?;
    anyhow::ensure!(
        fs::read(&quarantine)? == bytes,
        "preserved checkpoint changed during repair"
    );
    Ok(quarantine)
}

fn checkpoint_endpoints(state: &CheckpointRead) -> Option<(&str, &str)> {
    match state {
        Ok(CheckpointState::Valid(checkpoint)) => Some((&checkpoint.old, &checkpoint.new)),
        Ok(CheckpointState::Invalid {
            affected_profiles: Some((old, new)),
            ..
        }) => Some((old, new)),
        _ => None,
    }
}

fn checkpoint_conflicts(checkpoints: &CheckpointSet) -> (bool, std::collections::HashSet<String>) {
    let mut unknown_authority = false;
    let mut blocked_profiles = std::collections::HashSet::new();
    let mut profile_owners = std::collections::HashMap::new();
    for (_, state) in checkpoints {
        match state {
            Err(_) => unknown_authority = true,
            Ok(CheckpointState::Invalid {
                affected_profiles: Some((old, new)),
                ..
            }) => {
                blocked_profiles.insert(old.clone());
                blocked_profiles.insert(new.clone());
            }
            Ok(CheckpointState::Invalid {
                affected_profiles: None,
                ..
            }) => unknown_authority = true,
            _ => {}
        }
        if let Some((old, new)) = checkpoint_endpoints(state) {
            for profile in [old, new] {
                let count = profile_owners.entry(profile).or_insert(0usize);
                *count += 1;
            }
        }
    }
    for (profile, owners) in profile_owners {
        if owners > 1 {
            blocked_profiles.insert(profile.to_owned());
        }
    }

    // A conflict blocks the full connected checkpoint group.
    loop {
        let mut added_profile = false;
        for (_, state) in checkpoints {
            let Some((old, new)) = checkpoint_endpoints(state) else {
                continue;
            };
            if blocked_profiles.contains(old) || blocked_profiles.contains(new) {
                let old_added = blocked_profiles.insert(old.to_owned());
                let new_added = blocked_profiles.insert(new.to_owned());
                if old_added || new_added {
                    added_profile = true;
                }
            }
        }
        if !added_profile {
            break;
        }
    }

    (unknown_authority, blocked_profiles)
}

fn checkpoint_affects(checkpoint: &RenameCheckpoint, old: &str, new: &str) -> bool {
    checkpoint.old == old || checkpoint.old == new || checkpoint.new == old || checkpoint.new == new
}

fn prepare_rename_checkpoint(old: &str, new: &str) -> Result<PathBuf> {
    let checkpoints = read_checkpoint_set()?;
    let (unknown_authority, blocked_profiles) = checkpoint_conflicts(&checkpoints);
    if unknown_authority {
        let error = checkpoints.iter().find_map(|(_, state)| match state {
            Err(error) => Some(error.as_str()),
            Ok(CheckpointState::Invalid {
                error,
                affected_profiles: None,
                ..
            }) => Some(error.as_str()),
            _ => None,
        });
        anyhow::bail!(
            "profile rename checkpoint endpoints are unknown or unreadable; refusing rename: {}",
            error.unwrap_or("unknown checkpoint authority")
        );
    }
    anyhow::ensure!(
        !blocked_profiles.contains(old) && !blocked_profiles.contains(new),
        "profile rename conflicts with a pending checkpoint group"
    );

    for (path, state) in checkpoints {
        match state {
            Err(error) => anyhow::bail!(
                "profile rename checkpoint could not be read; refusing rename: {error}"
            ),
            Ok(CheckpointState::Missing) => {}
            Ok(CheckpointState::Valid(checkpoint)) => {
                if blocked_profiles.contains(&checkpoint.old)
                    || blocked_profiles.contains(&checkpoint.new)
                {
                    continue;
                }
                let affects_request = checkpoint_affects(&checkpoint, old, new);
                if let Err(error) = settle(&path, &checkpoint) {
                    anyhow::ensure!(
                        !affects_request,
                        "profile rename remains pending: {} -> {}: {error:#}",
                        checkpoint.old,
                        checkpoint.new
                    );
                    tracing::warn!(
                        target: "session.profile_rename",
                        checkpoint = %path.display(),
                        old = %checkpoint.old,
                        new = %checkpoint.new,
                        %error,
                        "unrelated profile rename is proceeding with its own checkpoint"
                    );
                }
            }
            Ok(CheckpointState::Invalid {
                error,
                affected_profiles: Some((pending_old, pending_new)),
                ..
            }) => {
                tracing::warn!(
                    target: "session.profile_rename",
                    checkpoint = %path.display(),
                    old = %pending_old,
                    new = %pending_new,
                    %error,
                    "unrelated profile rename is proceeding with its own checkpoint"
                );
            }
            Ok(CheckpointState::Invalid {
                error,
                affected_profiles: None,
                ..
            }) => anyhow::bail!(
                "profile rename checkpoint endpoints are unknown; refusing rename: {error}"
            ),
        }
    }

    let pending = active_checkpoint_paths()?;
    let path = if pending.is_empty() {
        checkpoint_path()?
    } else {
        sidecar_checkpoint_path(old, new)?
    };
    match fs::symlink_metadata(&path) {
        Ok(_) => anyhow::bail!(
            "profile rename checkpoint already exists: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(path)
}

fn settle_checkpoint(path: &Path) -> Result<()> {
    match read_checkpoint_at(path)? {
        CheckpointState::Missing => Ok(()),
        CheckpointState::Valid(checkpoint) => settle(path, &checkpoint),
        CheckpointState::Invalid { error, .. } => {
            anyhow::bail!("profile rename checkpoint needs repair: {error}")
        }
    }
}

pub(crate) fn ensure_profile_available(profile: &str) -> Result<()> {
    for path in active_checkpoint_paths()? {
        match read_checkpoint_at(&path)? {
            CheckpointState::Valid(checkpoint) => {
                anyhow::ensure!(
                    profile != checkpoint.old && profile != checkpoint.new,
                    "profile rename remains pending: {} -> {}",
                    checkpoint.old,
                    checkpoint.new
                );
            }
            CheckpointState::Invalid {
                affected_profiles: Some((old, new)),
                error,
                ..
            } => {
                anyhow::ensure!(
                    profile != old && profile != new,
                    "profile rename checkpoint needs repair for {old} -> {new}: {error}"
                );
            }
            CheckpointState::Invalid {
                error,
                affected_profiles: None,
                ..
            } => anyhow::bail!(
                "profile rename checkpoint endpoints are unknown; profile access is blocked: {error}"
            ),
            CheckpointState::Missing => {}
        }
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
    let profiles_dir = super::get_app_dir()?.join("profiles");
    let mut profile_sessions: Vec<PathBuf> = super::list_profile_names_in(&profiles_dir)?
        .into_iter()
        .map(|profile| profiles_dir.join(profile).join("sessions.json"))
        .collect();
    profile_sessions.extend([old_sessions.clone(), new_sessions.clone()]);
    super::move_journal::retarget_profile_references(
        profile_sessions,
        &checkpoint.old,
        &checkpoint.new,
        target,
    )?;
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

fn settle(path: &Path, checkpoint: &RenameCheckpoint) -> Result<()> {
    let old_dir = super::get_profile_dir_path(&checkpoint.old)?;
    let new_dir = super::get_profile_dir_path(&checkpoint.new)?;
    let old = crate::process::profile_directory_identity(&old_dir)?;
    let new = crate::process::profile_directory_identity(&new_dir)?;
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
        lifecycle_journal::consume(path)
    })
}

pub(crate) fn recover_once() -> Result<()> {
    let Some(_identity) = super::storage::try_acquire_session_identity_lock()? else {
        return Ok(());
    };
    let checkpoints = read_checkpoint_set()?;
    let (unknown_authority, blocked_profiles) = checkpoint_conflicts(&checkpoints);

    for (path, state) in checkpoints {
        match state {
            Err(error) => tracing::error!(
                target: "session.profile_rename",
                checkpoint = %path.display(),
                %error,
                "unreadable profile rename authority is retained and blocks checkpoint recovery"
            ),
            Ok(state) => match state {
                CheckpointState::Missing => {}
                CheckpointState::Valid(checkpoint) => {
                    if unknown_authority
                        || blocked_profiles.contains(&checkpoint.old)
                        || blocked_profiles.contains(&checkpoint.new)
                    {
                        tracing::warn!(
                            target: "session.profile_rename",
                            checkpoint = %path.display(),
                            old = %checkpoint.old,
                            new = %checkpoint.new,
                            unknown_authority,
                            "profile rename checkpoint conflicts with retained authority; recovery deferred"
                        );
                        continue;
                    }
                    if let Err(error) = settle(&path, &checkpoint) {
                        tracing::warn!(
                            target: "session.profile_rename",
                            checkpoint = %path.display(),
                            old = %checkpoint.old,
                            new = %checkpoint.new,
                            %error,
                            "profile rename remains pending; unrelated profiles may continue"
                        );
                    }
                }
                CheckpointState::Invalid {
                    bytes,
                    error,
                    affected_profiles: Some((old, new)),
                } => {
                    let sha256 = sha256_hex(&bytes);
                    tracing::warn!(
                        target: "session.profile_rename",
                        checkpoint = %path.file_name().unwrap_or(path.as_os_str()).to_string_lossy(),
                        sha256 = %sha256,
                        old = %old,
                        new = %new,
                        %error,
                        "profile rename checkpoint is retained for repair; unrelated profiles may continue; use `aoe profile repair-rename-checkpoint --checkpoint <filename> --sha256 <sha256> --old-profile <old> --new-profile <new>` after reviewing the preserved authority"
                    );
                }
                CheckpointState::Invalid {
                    bytes,
                    error,
                    affected_profiles: None,
                } => {
                    if is_quarantined_checkpoint(&path) {
                        tracing::error!(
                            target: "session.profile_rename",
                            checkpoint = %path.file_name().unwrap_or(path.as_os_str()).to_string_lossy(),
                            %error,
                            "unattributable profile rename authority remains active and blocks profile access"
                        );
                    } else if let Err(quarantine_error) = quarantine_checkpoint(&path, &bytes) {
                        return Err(quarantine_error).with_context(|| {
                            format!(
                            "unattributable profile rename checkpoint could not be preserved: {}",
                            path.display()
                        )
                        });
                    } else {
                        let sha256 = sha256_hex(&bytes);
                        tracing::error!(
                            target: "session.profile_rename",
                            checkpoint = %path.file_name().unwrap_or(path.as_os_str()).to_string_lossy(),
                            sha256 = %sha256,
                            %error,
                            "unattributable profile rename authority remains active; a repair copy was preserved; use `aoe profile repair-rename-checkpoint --checkpoint <filename> --sha256 <sha256>` after reviewing the preserved authority"
                        );
                    }
                }
            },
        }
    }
    Ok(())
}

pub(crate) fn rename(old: &str, new: &str) -> Result<()> {
    let _identity = super::storage::acquire_session_identity_lock()?;
    let path = prepare_rename_checkpoint(old, new)?;
    let old_dir = super::get_profile_dir_path(old)?;
    let new_dir = super::get_profile_dir_path(new)?;
    let storage = Storage::open_unwatched(old)?;
    let result = storage.with_profile_rename_locked(|| {
        anyhow::ensure!(!new_dir.try_exists()?, "Profile '{}' already exists", new);
        let directory = crate::process::profile_directory_identity(&old_dir)?
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
        atomic_write_verified(&path, &serde_json::to_vec(&checkpoint)?)?;
        sync_parent_directory(&path)?;
        retarget(&storage, &checkpoint, new)?;
        crate::process::rename_profile_directory(&old_dir, &new_dir)?;
        #[cfg(test)]
        super::storage::test_crash_point("profile-rename-after-directory");
        sync_parent_directory(&new_dir)?;
        finish_default(&checkpoint)?;
        lifecycle_journal::consume(&path)
    });
    if let Err(error) = result {
        if let Err(settlement) = settle_checkpoint(&path) {
            return Err(error.context(format!("rename settlement remains pending: {settlement:#}")));
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

    fn seed_purge(storage: &Storage, id: &str, workspace: &Path) {
        fs::create_dir_all(workspace).unwrap();
        fs::write(workspace.join("retained"), b"purge me").unwrap();
        let mut row = Instance::new(id, workspace.to_str().unwrap());
        row.id = id.to_owned();
        row.source_profile = storage.profile().to_owned();
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
        .with_phase(lifecycle_journal::LifecyclePhase::HooksComplete);
        lifecycle_journal::record(&entry).unwrap();
    }

    fn checkpoint_for(old: &str, new: &str) -> RenameCheckpoint {
        let storage = Storage::new_unwatched(old).unwrap();
        RenameCheckpoint {
            version: 1,
            old: old.to_owned(),
            new: new.to_owned(),
            directory: crate::process::profile_directory_identity(
                storage.sessions_path().parent().unwrap(),
            )
            .unwrap()
            .unwrap(),
            session_ids: Vec::new(),
            default_was_old: false,
        }
    }

    #[test]
    #[serial]
    fn malformed_checkpoint_blocks_unrelated_lifecycle_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let healthy = Storage::new_unwatched("healthy").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir("healthy-purge").unwrap();
        seed_purge(&healthy, "healthy-purge", &workspace);

        let raw = b"{ malformed checkpoint";
        fs::write(checkpoint_path().unwrap(), raw).unwrap();
        crate::session::deletion::recover_lifecycle_journals_once().unwrap();

        let digest = sha256_hex(raw);
        let quarantine = checkpoint_path()
            .unwrap()
            .with_file_name(format!("{CHECKPOINT}.corrupt-{digest}"));
        assert_eq!(fs::read(checkpoint_path().unwrap()).unwrap(), raw);
        assert_eq!(fs::read(&quarantine).unwrap(), raw);
        assert!(workspace.exists(), "unknown authority allowed a purge");
        assert_eq!(healthy.load().unwrap().len(), 1);
        assert!(ensure_profile_available("healthy").is_err());
        assert!(checkpoint_paths().unwrap().contains(&quarantine));
    }

    #[test]
    #[serial]
    fn unknown_authority_blocks_conflicting_valid_sidecar_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let source = Storage::new_unwatched("before").unwrap();
        let checkpoint = checkpoint_for("before", "after");
        let sidecar = sidecar_checkpoint_path("before", "after").unwrap();
        let bytes = serde_json::to_vec(&checkpoint).unwrap();
        fs::write(&sidecar, &bytes).unwrap();
        let raw = b"{ malformed checkpoint";
        fs::write(checkpoint_path().unwrap(), raw).unwrap();

        recover_once().unwrap();

        let quarantine = checkpoint_path()
            .unwrap()
            .with_file_name(format!("{CHECKPOINT}.corrupt-{}", sha256_hex(raw)));
        assert_eq!(fs::read(checkpoint_path().unwrap()).unwrap(), raw);
        assert_eq!(fs::read(quarantine).unwrap(), raw);
        assert_eq!(fs::read(&sidecar).unwrap(), bytes);
        assert!(source.sessions_path().parent().unwrap().is_dir());
        assert!(!super::super::get_profile_dir_path("after")
            .unwrap()
            .exists());
        assert!(ensure_profile_available("before").is_err());
    }

    #[test]
    #[serial]
    fn unreadable_checkpoint_blocks_valid_sidecar_recovery() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let source = Storage::new_unwatched("before").unwrap();
        let checkpoint = checkpoint_for("before", "after");
        let sidecar = sidecar_checkpoint_path("before", "after").unwrap();
        let bytes = serde_json::to_vec(&checkpoint).unwrap();
        fs::write(&sidecar, &bytes).unwrap();
        let primary = checkpoint_path().unwrap();
        fs::create_dir(&primary).unwrap();

        recover_once().unwrap();

        assert!(primary.is_dir());
        assert_eq!(fs::read(&sidecar).unwrap(), bytes);
        assert!(source.sessions_path().parent().unwrap().is_dir());
        assert!(!super::super::get_profile_dir_path("after")
            .unwrap()
            .exists());
        assert!(ensure_profile_available("before").is_err());
    }

    #[test]
    #[serial]
    fn shared_checkpoint_endpoint_defers_both_valid_renames() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let first_source = Storage::new_unwatched("before-a").unwrap();
        let second_source = Storage::new_unwatched("shared").unwrap();
        let first = checkpoint_for("before-a", "shared");
        let second = checkpoint_for("shared", "after-b");
        let primary = checkpoint_path().unwrap();
        let first_bytes = serde_json::to_vec(&first).unwrap();
        fs::write(&primary, &first_bytes).unwrap();
        let sidecar = sidecar_checkpoint_path("shared", "after-b").unwrap();
        let second_bytes = serde_json::to_vec(&second).unwrap();
        fs::write(&sidecar, &second_bytes).unwrap();

        recover_once().unwrap();

        assert_eq!(fs::read(&primary).unwrap(), first_bytes);
        assert_eq!(fs::read(&sidecar).unwrap(), second_bytes);
        assert!(first_source.sessions_path().parent().unwrap().is_dir());
        assert!(second_source.sessions_path().parent().unwrap().is_dir());
        assert!(!super::super::get_profile_dir_path("after-b")
            .unwrap()
            .exists());
        assert!(ensure_profile_available("before-a").is_err());
        assert!(ensure_profile_available("shared").is_err());
        assert!(ensure_profile_available("after-b").is_err());
    }

    #[test]
    #[serial]
    fn unrelated_rename_does_not_settle_one_checkpoint_from_conflict_group() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        Storage::new_unwatched("free-before").unwrap();
        let first = checkpoint_for("before-a", "shared");
        let second = checkpoint_for("shared", "after-b");
        let primary = checkpoint_path().unwrap();
        let first_bytes = serde_json::to_vec(&first).unwrap();
        fs::write(&primary, &first_bytes).unwrap();
        let sidecar = sidecar_checkpoint_path("shared", "after-b").unwrap();
        let second_bytes = serde_json::to_vec(&second).unwrap();
        fs::write(&sidecar, &second_bytes).unwrap();

        assert!(rename("shared", "after-c").is_err());
        assert!(!super::super::get_profile_dir_path("after-c")
            .unwrap()
            .exists());
        rename("free-before", "free-after").unwrap();

        assert_eq!(fs::read(&primary).unwrap(), first_bytes);
        assert_eq!(fs::read(&sidecar).unwrap(), second_bytes);
        assert!(super::super::get_profile_dir_path("shared")
            .unwrap()
            .is_dir());
        assert!(!super::super::get_profile_dir_path("after-b")
            .unwrap()
            .exists());
        assert!(super::super::get_profile_dir_path("free-after")
            .unwrap()
            .is_dir());
        assert!(!super::super::get_profile_dir_path("free-before")
            .unwrap()
            .exists());
    }

    #[test]
    #[serial]
    fn attributable_invalid_checkpoint_blocks_only_conflicting_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        Storage::new_unwatched("free-before").unwrap();
        rename("free-before", "free-after").unwrap();
        Storage::new_unwatched("blocked-before").unwrap();
        Storage::new_unwatched("blocked-after").unwrap();
        let invalid = serde_json::json!({
            "version": 1,
            "old": "blocked-before",
            "new": "blocked-after"
        });
        let primary = checkpoint_path().unwrap();
        let invalid_bytes = serde_json::to_vec(&invalid).unwrap();

        let blocked_checkpoint = checkpoint_for("blocked-after", "blocked-final");
        let blocked_path = sidecar_checkpoint_path("blocked-after", "blocked-final").unwrap();
        let blocked_bytes = serde_json::to_vec(&blocked_checkpoint).unwrap();
        fs::write(&blocked_path, &blocked_bytes).unwrap();

        let unrelated_checkpoint = RenameCheckpoint {
            version: 1,
            old: "free-before".to_owned(),
            new: "free-after".to_owned(),
            directory: crate::process::profile_directory_identity(
                &super::super::get_profile_dir_path("free-after").unwrap(),
            )
            .unwrap()
            .unwrap(),
            session_ids: Vec::new(),
            default_was_old: false,
        };
        let unrelated_path = sidecar_checkpoint_path("free-before", "free-after").unwrap();
        fs::write(
            &unrelated_path,
            serde_json::to_vec(&unrelated_checkpoint).unwrap(),
        )
        .unwrap();

        fs::write(&primary, &invalid_bytes).unwrap();
        recover_once().unwrap();

        assert_eq!(fs::read(&primary).unwrap(), invalid_bytes);
        assert_eq!(fs::read(&blocked_path).unwrap(), blocked_bytes);
        assert!(!unrelated_path.exists());
        assert!(super::super::get_profile_dir_path("free-after")
            .unwrap()
            .is_dir());
        assert!(!super::super::get_profile_dir_path("free-before")
            .unwrap()
            .exists());
        assert!(ensure_profile_available("blocked-before").is_err());
        assert!(ensure_profile_available("blocked-after").is_err());
        assert!(ensure_profile_available("free-after").is_ok());
    }

    #[test]
    #[serial]
    fn quarantined_checkpoint_copy_is_evidence_not_active_authority() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let healthy = Storage::new_unwatched("healthy").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir("healthy-purge").unwrap();
        seed_purge(&healthy, "healthy-purge", &workspace);

        let raw = serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "old": "broken-before",
            "new": "broken-after"
        }))
        .unwrap();
        let digest = sha256_hex(&raw);
        let sidecar = checkpoint_path()
            .unwrap()
            .with_file_name(format!("{CHECKPOINT}.corrupt-{digest}"));
        fs::write(&sidecar, &raw).unwrap();

        recover_once().unwrap();
        assert!(!checkpoint_path().unwrap().exists());
        assert_eq!(fs::read(&sidecar).unwrap(), raw);
        assert!(ensure_profile_available("broken-before").is_ok());
        assert!(ensure_profile_available("broken-after").is_ok());
        assert!(ensure_profile_available("healthy").is_ok());

        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(!workspace.exists(), "healthy profile purge did not recover");
        assert!(healthy.load().unwrap().is_empty());
        assert_eq!(fs::read(&sidecar).unwrap(), raw);
    }

    #[test]
    #[serial]
    fn unattributable_checkpoint_repair_checks_digest_preserves_evidence_and_retries() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let source = Storage::new_unwatched("before").unwrap();
        let unrelated = checkpoint_for("free-before", "free-after");
        let unrelated_path = sidecar_checkpoint_path("free-before", "free-after").unwrap();
        fs::write(&unrelated_path, serde_json::to_vec(&unrelated).unwrap()).unwrap();

        let raw = b"{ malformed checkpoint";
        let active = checkpoint_path().unwrap();
        fs::write(&active, raw).unwrap();
        let digest = sha256_hex(raw);
        let quarantine =
            active.with_file_name(format!("{CHECKPOINT}{CORRUPT_CHECKPOINT_MARKER}{digest}"));

        recover_once().unwrap();
        assert!(ensure_profile_available("before").is_err());
        assert!(ensure_profile_available("healthy").is_err());
        assert_eq!(fs::read(&active).unwrap(), raw);
        assert_eq!(fs::read(&quarantine).unwrap(), raw);
        assert!(
            unrelated_path.exists(),
            "unknown authority must defer valid checkpoints"
        );

        let wrong_digest = "0".repeat(64);
        assert!(repair_rename_checkpoint(CHECKPOINT, &wrong_digest, None).is_err());
        assert_eq!(fs::read(&active).unwrap(), raw);
        assert_eq!(fs::read(&quarantine).unwrap(), raw);

        let repaired = repair_rename_checkpoint(CHECKPOINT, &digest.to_uppercase(), None).unwrap();
        assert_eq!(repaired, quarantine);
        assert!(!active.exists());
        assert_eq!(fs::read(&repaired).unwrap(), raw);

        let retried = repair_rename_checkpoint(CHECKPOINT, &digest, None).unwrap();
        assert_eq!(retried, repaired);
        recover_once().unwrap();
        assert!(!unrelated_path.exists());
        assert!(super::super::get_profile_dir_path("free-before")
            .unwrap()
            .is_dir());
        assert!(ensure_profile_available("before").is_ok());
        assert!(ensure_profile_available("healthy").is_ok());
        assert_eq!(fs::read(&repaired).unwrap(), raw);
        assert!(source.sessions_path().parent().unwrap().is_dir());
    }

    #[test]
    #[serial]
    fn attributable_invalid_checkpoint_repair_requires_matching_endpoints_and_preserves_evidence() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let active = checkpoint_path().unwrap();
        let raw = br#"{"version":1,"old":"before","new":"after","session_ids":"invalid"}"#;
        fs::write(&active, raw).unwrap();
        let digest = sha256_hex(raw);
        let quarantine =
            active.with_file_name(format!("{CHECKPOINT}{CORRUPT_CHECKPOINT_MARKER}{digest}"));

        recover_once().unwrap();
        assert!(ensure_profile_available("before").is_err());
        assert!(ensure_profile_available("after").is_err());
        assert!(ensure_profile_available("healthy").is_ok());

        assert!(repair_rename_checkpoint(CHECKPOINT, &digest, None).is_err());
        assert!(repair_rename_checkpoint(CHECKPOINT, &digest, Some(("before", "wrong"))).is_err());
        assert_eq!(fs::read(&active).unwrap(), raw);
        assert!(!quarantine.exists());

        let preserved =
            repair_rename_checkpoint(CHECKPOINT, &digest, Some(("before", "after"))).unwrap();
        assert_eq!(preserved, quarantine);
        assert!(!active.exists());
        assert_eq!(fs::read(&preserved).unwrap(), raw);
        assert!(repair_rename_checkpoint(CHECKPOINT, &digest, None).is_err());
        assert!(repair_rename_checkpoint(CHECKPOINT, &digest, Some(("wrong", "after"))).is_err());
        assert_eq!(
            repair_rename_checkpoint(CHECKPOINT, &digest, Some(("before", "after"))).unwrap(),
            preserved
        );

        recover_once().unwrap();
        assert!(ensure_profile_available("before").is_ok());
        assert!(ensure_profile_available("after").is_ok());
        assert!(ensure_profile_available("healthy").is_ok());
        assert_eq!(fs::read(&preserved).unwrap(), raw);
    }

    #[test]
    #[serial]
    fn quarantine_failure_keeps_unrelated_lifecycle_recovery_fail_closed() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let healthy = Storage::new_unwatched("healthy").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir("healthy-purge").unwrap();
        seed_purge(&healthy, "healthy-purge", &workspace);

        let raw = b"{ malformed checkpoint";
        let path = checkpoint_path().unwrap();
        fs::write(&path, raw).unwrap();
        let digest = sha256_hex(raw);
        let quarantine = path.with_file_name(format!("{CHECKPOINT}.corrupt-{digest}"));
        fs::write(&quarantine, b"different checkpoint bytes").unwrap();

        assert!(ensure_profile_available("healthy").is_err());
        assert!(crate::session::deletion::recover_lifecycle_journals_once().is_err());

        assert_eq!(fs::read(&path).unwrap(), raw);
        assert_eq!(fs::read(quarantine).unwrap(), b"different checkpoint bytes");
        assert!(
            workspace.exists(),
            "lifecycle recovery ran without quarantine"
        );
        assert_eq!(healthy.load().unwrap().len(), 1);
    }

    #[test]
    #[serial]
    fn ambiguous_checkpoint_is_scoped_and_recovers_after_directory_conflict_is_removed() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let old = Storage::new_unwatched("before").unwrap();
        let healthy = Storage::new_unwatched("healthy").unwrap();
        let workspace = crate::session::scratch::provision_scratch_dir("healthy-purge").unwrap();
        seed_purge(&healthy, "healthy-purge", &workspace);
        let old_marker = old.sessions_path().parent().unwrap().join("retained");
        fs::write(&old_marker, b"original profile data").unwrap();

        let new_dir = super::super::get_profile_dir_path("after").unwrap();
        fs::create_dir(&new_dir).unwrap();
        let checkpoint = RenameCheckpoint {
            version: 1,
            old: "before".to_owned(),
            new: "after".to_owned(),
            directory: crate::process::profile_directory_identity(
                old.sessions_path().parent().unwrap(),
            )
            .unwrap()
            .unwrap(),
            session_ids: Vec::new(),
            default_was_old: false,
        };
        fs::write(
            checkpoint_path().unwrap(),
            serde_json::to_vec(&checkpoint).unwrap(),
        )
        .unwrap();

        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(
            !workspace.exists(),
            "unrelated profile recovery was blocked"
        );
        assert!(checkpoint_path().unwrap().exists());
        assert!(ensure_profile_available("before").is_err());
        assert!(ensure_profile_available("after").is_err());
        assert!(ensure_profile_available("healthy").is_ok());

        fs::remove_dir_all(&new_dir).unwrap();
        crate::session::deletion::recover_lifecycle_journals_once().unwrap();
        assert!(!checkpoint_path().unwrap().exists());
        assert!(ensure_profile_available("before").is_ok());
        assert_eq!(fs::read(old_marker).unwrap(), b"original profile data");
        assert!(healthy.load().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn unrelated_rename_keeps_an_unresolved_checkpoint_and_uses_a_sidecar() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let pending = Storage::new_unwatched("pending-before").unwrap();
        Storage::new_unwatched("unrelated-before").unwrap();
        let conflicting_dir = super::super::get_profile_dir_path("pending-after").unwrap();
        fs::create_dir(&conflicting_dir).unwrap();
        let checkpoint = RenameCheckpoint {
            version: 1,
            old: "pending-before".to_owned(),
            new: "pending-after".to_owned(),
            directory: crate::process::profile_directory_identity(
                pending.sessions_path().parent().unwrap(),
            )
            .unwrap()
            .unwrap(),
            session_ids: Vec::new(),
            default_was_old: false,
        };
        let primary = checkpoint_path().unwrap();
        let original = serde_json::to_vec(&checkpoint).unwrap();
        fs::write(&primary, &original).unwrap();

        rename("unrelated-before", "unrelated-after").unwrap();

        assert_eq!(fs::read(&primary).unwrap(), original);
        assert!(super::super::get_profile_dir_path("unrelated-after")
            .unwrap()
            .is_dir());
        assert!(!super::super::get_profile_dir_path("unrelated-before")
            .unwrap()
            .exists());
        assert!(ensure_profile_available("pending-before").is_err());
        assert!(ensure_profile_available("pending-after").is_err());
        assert_eq!(checkpoint_paths().unwrap(), vec![primary]);
    }

    #[test]
    #[serial]
    fn rename_fails_closed_when_checkpoint_endpoints_are_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let storage = Storage::new_unwatched("before").unwrap();
        let raw = b"{ malformed checkpoint";
        fs::write(checkpoint_path().unwrap(), raw).unwrap();

        assert!(rename("before", "after").is_err());

        assert!(storage.sessions_path().parent().unwrap().is_dir());
        assert!(!super::super::get_profile_dir_path("after")
            .unwrap()
            .exists());
        assert_eq!(fs::read(checkpoint_path().unwrap()).unwrap(), raw);
        assert_eq!(
            checkpoint_paths().unwrap(),
            vec![checkpoint_path().unwrap()]
        );
    }

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

    #[cfg(unix)]
    #[test]
    #[serial]
    fn interrupted_rename_retargets_external_move_journals_during_recovery() {
        for (point, expected_profile) in [
            ("profile-rename-after-journals", "before"),
            ("profile-rename-after-directory", "after"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let before = Storage::new_unwatched("before").unwrap();
            let source = Storage::new_unwatched("source").unwrap();
            let destination = Storage::new_unwatched("destination").unwrap();
            let entry = super::super::move_journal::MoveJournalEntry {
                version: super::super::move_journal::MOVE_JOURNAL_VERSION,
                ids: vec!["moving-session".to_string()],
                source_profile: "source".to_string(),
                target_profile: "before".to_string(),
                source_sessions_path: source.sessions_path().to_path_buf(),
                target_sessions_path: before.sessions_path().to_path_buf(),
                group_move_source_path: "group".to_string(),
                group_move_target_path: "group".to_string(),
                group_move_subtree: false,
                created_at_epoch_ms: 7,
            };
            let journal_path =
                super::super::move_journal::record(&entry, source.sessions_path()).unwrap();
            let mut local_entry = entry.clone();
            local_entry.ids = vec!["local-moving-session".to_string()];
            local_entry.source_profile = "before".to_string();
            local_entry.target_profile = "destination".to_string();
            local_entry.source_sessions_path = before.sessions_path().to_path_buf();
            local_entry.target_sessions_path = destination.sessions_path().to_path_buf();
            let local_journal_path =
                super::super::move_journal::record(&local_entry, before.sessions_path()).unwrap();

            let interrupted = {
                let _point = super::super::storage::ArmedCrashPoint::arm(point);
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    crate::session::rename_profile("before", "after")
                }))
            };
            assert!(interrupted.is_err(), "crash point {point}");
            assert!(checkpoint_path().unwrap().exists());
            recover_once().unwrap();

            let scan = super::super::move_journal::scan([source.sessions_path().to_path_buf()]);
            let (recovered_path, recovered) = scan.entries.into_iter().next().unwrap();
            let recovered = recovered.unwrap();
            assert_eq!(recovered_path, journal_path);
            assert_eq!(recovered.target_profile, expected_profile);
            assert_eq!(
                recovered.target_sessions_path,
                super::super::get_profile_dir_path(expected_profile)
                    .unwrap()
                    .join("sessions.json")
            );
            assert_eq!(recovered.version, entry.version);
            assert_eq!(recovered.ids, entry.ids);
            assert_eq!(recovered.created_at_epoch_ms, entry.created_at_epoch_ms);

            let renamed_storage = Storage::open_unwatched(expected_profile).unwrap();
            let local_scan =
                super::super::move_journal::scan([renamed_storage.sessions_path().to_path_buf()]);
            let (recovered_local_path, recovered_local) =
                local_scan.entries.into_iter().next().unwrap();
            let recovered_local = recovered_local.unwrap();
            assert_eq!(
                recovered_local_path.file_name(),
                local_journal_path.file_name()
            );
            assert_eq!(
                recovered_local_path.parent(),
                Some(
                    renamed_storage
                        .sessions_path()
                        .parent()
                        .unwrap()
                        .join(".move-journal")
                        .as_path()
                )
            );
            assert_eq!(recovered_local.source_profile, expected_profile);
            assert_eq!(
                recovered_local.source_sessions_path,
                renamed_storage.sessions_path()
            );
            assert_eq!(recovered_local.target_profile, "destination");
            assert!(!checkpoint_path().unwrap().exists());
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
                target
                    .update(|rows, _| {
                        let row = rows
                            .iter_mut()
                            .find(|row| row.id == "renamed")
                            .expect("recovery must restore the row from its journal");
                        row.lifecycle_reservation.as_mut().unwrap().at =
                            chrono::Utc::now() - chrono::Duration::days(1);
                        Ok(())
                    })
                    .unwrap();
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
