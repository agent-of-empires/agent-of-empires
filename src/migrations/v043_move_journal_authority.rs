//! Released v1 intents remain evidence, never authority over a session or checkout.

use anyhow::{Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{fs, path::PathBuf};

use crate::session::environment::shell_escape_script_word;
use crate::session::move_journal::{
    JournalScope, MoveAuthority, MoveJournalEntry, ProfileAuthority, MOVE_JOURNAL_VERSION,
};

#[derive(Deserialize)]
struct LegacyEntry {
    version: u32,
    ids: Vec<String>,
    source_profile: String,
    target_profile: String,
    source_sessions_path: PathBuf,
    target_sessions_path: PathBuf,
    group_move_source_path: String,
    group_move_target_path: String,
    group_move_subtree: bool,
    created_at_epoch_ms: u64,
}

pub fn run() -> Result<()> {
    let ownership = crate::session::storage::acquire_ownership_lock()?;
    let directories = crate::session::move_journal::journal_directories()?;
    let central = ownership.app_dir().join(".move-journal");
    let mut journals = Vec::new();
    for directory in directories {
        anyhow::ensure!(
            !fs::symlink_metadata(&directory)
                .is_ok_and(|metadata| metadata.file_type().is_symlink()),
            "journal directory is an alias: {}; inspect with `aoe intents list`",
            directory.display()
        );
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(fs::symlink_metadata(&directory), Err(missing) if missing.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue
            }
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "listing {}; inspect with `aoe intents list`",
                        directory.display()
                    )
                })
            }
        };
        for entry in entries {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                anyhow::ensure!(
                    entry.file_type()?.is_file(),
                    "journal is not a regular file: {}",
                    entry.path().display()
                );
                journals.push(entry.path());
            }
        }
    }
    journals.sort();
    for path in journals {
        let bytes = fs::read(&path)?;
        let raw: serde_json::Value =
            serde_json::from_slice(&bytes).with_context(|| remedy(&path))?;
        if raw.get("version").and_then(serde_json::Value::as_u64)
            == Some(u64::from(MOVE_JOURNAL_VERSION))
        {
            let current: MoveJournalEntry =
                serde_json::from_value(raw).with_context(|| remedy(&path))?;
            anyhow::ensure!(current.is_current(), "{}", remedy(&path));
            continue;
        }
        let legacy: LegacyEntry =
            serde_json::from_value(raw.clone()).with_context(|| remedy(&path))?;
        anyhow::ensure!(
            legacy.version == 1,
            "unsupported move-journal version {} at {}; {}",
            legacy.version,
            path.display(),
            remedy(&path)
        );
        anyhow::ensure!(
            crate::session::move_journal::lease_state(&path)?
                != crate::session::move_journal::LeaseState::Live,
            "live legacy intent at {}; {}",
            path.display(),
            remedy(&path)
        );
        let source = ProfileAuthority {
            profile: legacy.source_profile.clone(),
            sessions_path: crate::session::deletion::resolve_claim_path(
                &legacy.source_sessions_path,
            )
            .context("unresolvable legacy source")?,
        };
        let target = ProfileAuthority {
            profile: legacy.target_profile.clone(),
            sessions_path: crate::session::deletion::resolve_claim_path(
                &legacy.target_sessions_path,
            )
            .context("unresolvable legacy target")?,
        };
        let migrated = MoveJournalEntry {
            version: MOVE_JOURNAL_VERSION,
            ids: legacy.ids,
            source_profile: legacy.source_profile,
            target_profile: legacy.target_profile,
            source_sessions_path: source.sessions_path.clone(),
            target_sessions_path: target.sessions_path.clone(),
            group_move_source_path: legacy.group_move_source_path,
            group_move_target_path: legacy.group_move_target_path,
            group_move_subtree: legacy.group_move_subtree,
            created_at_epoch_ms: legacy.created_at_epoch_ms,
            authority: MoveAuthority::LegacyConflict {
                legacy: raw,
                reason: "released v1 intent has no incarnation, generation or physical-resource authority".into(),
                scope: JournalScope { profiles: vec![source, target], resources: Vec::new() },
            },
        };
        let mut hash = Sha256::new();
        hash.update(path.canonicalize()?.as_os_str().as_encoded_bytes());
        hash.update(&bytes);
        use std::fmt::Write as _;
        let digest = hash.finalize();
        let mut name = String::with_capacity("legacy-".len() + digest.len() * 2 + ".json".len());
        name.push_str("legacy-");
        for byte in digest {
            write!(&mut name, "{byte:02x}")?;
        }
        name.push_str(".json");
        let destination = central.join(name);
        let migrated_bytes = serde_json::to_vec_pretty(&migrated)?;
        crate::session::backup_before_migration(&path)?;
        crate::session::storage::sync_parent_directory(&path)?;
        fs::create_dir_all(&central)?;
        crate::session::storage::sync_parent_directory(&central)?;
        crate::session::move_journal::create_migration_lease(&destination)?;
        match fs::symlink_metadata(&destination) {
            Ok(metadata) => anyhow::ensure!(
                metadata.is_file()
                    && !metadata.file_type().is_symlink()
                    && fs::read(&destination)? == migrated_bytes,
                "migration destination contains different evidence at {}; {}",
                destination.display(),
                remedy(&destination)
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                crate::session::storage::atomic_write_verified(&destination, &migrated_bytes)?
            }
            Err(error) => return Err(error.into()),
        }
        crate::session::storage::sync_parent_directory(&destination)?;
        anyhow::ensure!(
            fs::read(&destination)? == migrated_bytes,
            "central legacy conflict was not verified"
        );
        fs::remove_file(&path)?;
        match fs::remove_file(path.with_extension("lock")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        crate::session::storage::sync_parent_directory(&path)?;
        tracing::info!(target: "migrations", source = %path.display(), destination = %destination.display(), "preserved released move intent as a conflict");
    }
    Ok(())
}

fn remedy(path: &std::path::Path) -> String {
    format!("cannot migrate {}; inspect with `aoe intents list`; resolve with `aoe intents clear {} --actors-stopped`", path.display(), shell_escape_script_word(&path.to_string_lossy()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn released_intent_migration_preserves_stores_and_raw_evidence() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("owner")?;
        let target = crate::session::Storage::new_unwatched("target")?;
        storage.update(|rows, _| {
            rows.push(crate::session::Instance::new("original", "/original"));
            Ok(())
        })?;
        let before = fs::read(storage.sessions_path())?;
        let directory = storage
            .sessions_path()
            .parent()
            .unwrap()
            .join(".move-journal");
        fs::create_dir(&directory)?;
        let path = directory.join("move-1-2.json");
        let old = serde_json::json!({
            "version": 1, "ids": ["original-id"], "source_profile": "owner", "target_profile": "target",
            "source_sessions_path": storage.sessions_path(), "target_sessions_path": target.sessions_path(),
            "group_move_source_path": "before", "group_move_target_path": "after", "group_move_subtree": false,
            "created_at_epoch_ms": 123, "unknown_original_field": "preserved"
        });
        let bytes = serde_json::to_vec_pretty(&old)?;
        fs::write(&path, &bytes)?;
        use fs2::FileExt;
        crate::session::move_journal::create_migration_lease(&path)?;
        let live = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        live.lock_exclusive()?;
        assert!(run().is_err());
        assert_eq!(fs::read(&path)?, bytes);
        drop(live);
        run()?;
        assert!(!path.exists());
        assert_eq!(fs::read(storage.sessions_path())?, before);
        let scanned = crate::session::move_journal::scan();
        let (central, entry) = &scanned.entries[0];
        let entry = entry.as_ref().map_err(|error| anyhow::anyhow!("{error}"))?;
        let MoveAuthority::LegacyConflict { legacy, scope, .. } = &entry.authority else {
            panic!("expected conflict")
        };
        assert_eq!(legacy, &old);
        assert!(scope.resources.is_empty());
        assert!(entry.affects_profile(&ProfileAuthority::capture(&target)?));
        assert_eq!(
            crate::session::move_journal::lease_state(central)?,
            crate::session::move_journal::LeaseState::Free
        );
        let backups = crate::session::storage::migration_backups(&path)?;
        assert_eq!(fs::read(&backups[0].1)?, bytes);
        let first = fs::read(central)?;
        run()?;
        assert_eq!(fs::read(central)?, first);
        crate::cli::intents::run(
            "owner",
            crate::cli::intents::IntentCommands::Clear {
                path: Some(central.clone()),
                orphan_binding: None,
                actors_stopped: true,
                reason: None,
            },
        )?;
        assert!(!central.exists());
        assert_eq!(fs::read(storage.sessions_path())?, before);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn unsupported_or_malformed_journals_remain_available_to_operator() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("owner")?;
        let directory = storage
            .sessions_path()
            .parent()
            .unwrap()
            .join(".move-journal");
        fs::create_dir(&directory)?;
        let path = directory.join("unsupported.json");
        for bytes in [b"{broken".as_slice(), b"{\"version\":2}".as_slice()] {
            fs::write(&path, bytes)?;
            assert!(run().is_err());
            assert_eq!(fs::read(&path)?, bytes);
            let inventory = crate::session::move_journal::operator_inventory()?;
            assert!(inventory
                .entries
                .iter()
                .any(|(found, result)| found == &path && result.is_err()));
        }
        Ok(())
    }
}
