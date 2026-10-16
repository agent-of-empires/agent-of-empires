//! Explicit, non-destructive resolution of interrupted resource intents.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Subcommand;

use crate::session::environment::shell_escape_script_word;
use crate::session::move_journal::{self, MoveAuthority, MoveJournalEntry};
use crate::session::storage::{self, OwnershipGuard};
use crate::session::Storage;

#[derive(Subcommand)]
pub enum IntentCommands {
    /// List current, legacy and unreadable intents and orphaned session bindings
    List,
    /// Archive and abandon one intent without deleting any checkout, branch or container
    Clear {
        /// Exact JSON path printed by `aoe intents list`
        #[arg(
            conflicts_with = "orphan_binding",
            required_unless_present = "orphan_binding"
        )]
        path: Option<PathBuf>,
        /// Release only this session's reservation when its referenced JSON is missing
        #[arg(long, conflicts_with = "path", required_unless_present = "path")]
        orphan_binding: Option<String>,
        /// Attest that AoE producers and surviving Git/hook children are stopped and affected state has been inspected and reconciled. A free parent lease does not establish this
        #[arg(long)]
        actors_stopped: bool,
        /// Operator's explanation, printed with the resolution receipt
        #[arg(long)]
        reason: Option<String>,
    },
}

pub fn run(profile: &str, command: IntentCommands) -> Result<()> {
    match command {
        IntentCommands::List => list(),
        IntentCommands::Clear {
            path,
            orphan_binding,
            actors_stopped,
            reason,
        } => {
            anyhow::ensure!(actors_stopped,
                "clear requires --actors-stopped: stop AoE producers and surviving Git/hook children, then inspect and reconcile affected state; a free parent lease is not proof");
            anyhow::ensure!(
                path.is_some() != orphan_binding.is_some(),
                "select exactly one journal path or --orphan-binding session id"
            );
            let root = storage::acquire_ownership_lock()?;
            if let Some(path) = path {
                clear_path(&root, &path)?;
            } else if let Some(id) = orphan_binding {
                crate::session::validate_instance_id(&id)?;
                let profile = crate::session::resolve_existing_profile(profile)?;
                let storage = Storage::open_with_ownership(
                    &profile,
                    crate::file_watch::FileWatchService::noop(),
                    &root,
                )?;
                let row = storage
                    .load_strict_for_worktree_ownership()?
                    .into_iter()
                    .find(|row| row.id == id)
                    .context("orphan-binding session not found")?;
                storage.release_attested_binding_with_ownership(&root, &row)?;
                println!("Cleared orphan binding for profile {profile}, session {id}; no physical resources changed");
            }
            if let Some(reason) = reason {
                println!("Operator reason: {reason}");
            }
            report_orphan_bindings(&root)?;
            Ok(())
        }
    }
}

fn list() -> Result<()> {
    let root = storage::acquire_ownership_read()?;
    let inventory = move_journal::operator_inventory()?;
    for (path, reason) in inventory.unreadable_dirs {
        println!("{}: unreadable directory: {reason}", path.display());
    }
    for (path, parsed) in inventory.entries {
        let lease = match move_journal::lease_state(&path) {
            Ok(state) => format!("{state:?}"),
            Err(error) => format!("unreadable: {error:#}"),
        };
        match parsed {
            Ok(entry) => println!("{}: lease={lease}; {:?}", path.display(), entry.authority),
            Err(error) => {
                println!(
                    "{}: lease={lease}; raw legacy, foreign or malformed entry: {error}",
                    path.display()
                );
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        println!("  raw: {}", String::from_utf8_lossy(&bytes).escape_debug())
                    }
                    Err(error) => println!("  read failed: {error}"),
                }
            }
        }
    }
    report_orphan_bindings(&root)
}

fn clear_path(root: &OwnershipGuard, path: &Path) -> Result<()> {
    let (journal, original) = move_journal::resolution_lease(root, path)?;
    storage::backup_before_repair(&journal.path)?;
    anyhow::ensure!(
        std::fs::read(&journal.path)? == original,
        "journal changed after backup"
    );
    let typed = serde_json::from_slice::<MoveJournalEntry>(&original)
        .ok()
        .filter(MoveJournalEntry::is_current);
    if let Some(mut entry) = typed {
        match &mut entry.authority {
            MoveAuthority::Resource { .. } => {
                let source = Storage::open_with_ownership(
                    &entry.source_profile,
                    crate::file_watch::FileWatchService::noop(),
                    root,
                );
                storage::abandon_resource_journal_with_ownership(root, &journal, entry, source)?;
            }
            MoveAuthority::Cleanup {
                abandoned,
                conflict,
                ..
            } => {
                *abandoned = true;
                *conflict = None;
                journal.rewrite(&entry)?;
                journal.consume()?;
            }
            MoveAuthority::Metadata { .. } | MoveAuthority::LegacyConflict { .. } => {
                journal.consume()?
            }
        }
    } else {
        journal.consume()?;
    }
    println!(
        "Archived and cleared {}; no physical resources changed",
        journal.path.display()
    );
    Ok(())
}

fn report_orphan_bindings(root: &OwnershipGuard) -> Result<()> {
    for profile in crate::session::list_profiles_for_worktree_inventory()? {
        let storage = Storage::open_with_ownership(
            &profile,
            crate::file_watch::FileWatchService::noop(),
            root,
        )?;
        for row in storage.load_strict_for_worktree_ownership()? {
            let Some(reservation) = &row.lifecycle_reservation else {
                continue;
            };
            let Some(binding) = &reservation.resource_intent else {
                continue;
            };
            let path = move_journal::binding_path(binding)?;
            if matches!(std::fs::symlink_metadata(&path), Err(error) if error.kind() == std::io::ErrorKind::NotFound)
            {
                println!("Orphan binding: profile={profile} session={} operation={:?} generation={} journal={binding}; lease={:?}", row.id, reservation.op, reservation.generation, move_journal::lease_state(&path)?);
                println!(
                    "  aoe --profile={} intents clear --orphan-binding={} --actors-stopped",
                    shell_escape_script_word(&profile),
                    row.id
                );
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Instance;

    fn pending_fixture() -> Result<(Storage, Instance, crate::session::claim::ReservedResource)> {
        let storage = Storage::new_unwatched("owner")?;
        let before = Instance::new("owner", "/unchanged");
        storage.update(|rows, _| {
            rows.push(before.clone());
            Ok(())
        })?;
        let mut reserved = before.clone();
        let generation = reserved.try_acquire_lifecycle_reservation(
            crate::session::LifecycleOperation::Rename,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )?;
        let mut after = reserved.clone();
        after.lifecycle_reservation = None;
        let profile = move_journal::ProfileAuthority::capture(&storage)?;
        let intent = move_journal::ResourceIntent::new(
            move_journal::IntentOwner {
                profile: profile.clone(),
                id: before.id.clone(),
                created_at: before.created_at,
                generation,
            },
            profile,
            crate::session::LifecycleOperation::Rename,
            Some(before.clone()),
            Some(after),
            vec![move_journal::ResourceEffect::Callback {
                name: "interrupted-hook".into(),
                directory: "/unchanged".into(),
                checkouts: Vec::new(),
            }],
            Vec::new(),
        )?;
        let root = storage::acquire_ownership_lock()?;
        let admitted = crate::session::claim::ResourceReservation::admit_with_ownership(
            &storage, &root, intent, reserved,
        )?;
        let mut entry = admitted.transaction.journal().read()?;
        let MoveAuthority::Resource { intent } = &mut entry.authority else {
            unreachable!()
        };
        intent.effects[0].phase = move_journal::EffectPhase::Pending;
        intent.phase = move_journal::EffectPhase::Pending;
        admitted.transaction.journal().rewrite(&entry)?;
        Ok((storage, before, admitted))
    }

    #[test]
    #[serial_test::serial]
    fn pending_resolution_validates_authority_and_preserves_session_fields() -> Result<()> {
        for change in ["none", "incarnation", "generation", "other-intent"] {
            let temp = tempfile::tempdir()?;
            let home = temp.path().join("home");
            std::fs::create_dir(&home)?;
            let alias = temp.path().join("home-alias");
            std::os::unix::fs::symlink(&home, &alias)?;
            let _app = crate::session::test_support::isolate_app_dir_at(&alias);
            let (storage, before, admitted) = pending_fixture()?;
            let path = admitted.transaction.journal().path.clone();
            assert!(
                run("owner", clear(&path, true)).is_err(),
                "live producer must refuse"
            );
            drop(admitted);
            assert!(
                !Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())?
                    .is_empty()
            );
            assert!(
                path.exists(),
                "equal snapshots cannot retire a Pending effect"
            );
            let mut rows = storage.load()?;
            let row = &mut rows[0];
            row.group_path = "kept-group".into();
            row.status = crate::session::Status::Stopped;
            match change {
                "incarnation" => row.created_at += chrono::Duration::seconds(1),
                "generation" => row.lifecycle_generation += 1,
                "other-intent" => {
                    row.lifecycle_reservation.as_mut().unwrap().resource_intent =
                        Some("other.json".into())
                }
                _ => {}
            }
            std::fs::write(storage.sessions_path(), serde_json::to_vec_pretty(&rows)?)?;
            let original_journal = std::fs::read(&path)?;
            let original_store = std::fs::read(storage.sessions_path())?;
            let result = run("owner", clear(&path, true));
            if change == "none" {
                result?;
                assert!(!path.exists());
                let mut expected = rows.remove(0);
                expected.lifecycle_reservation = None;
                assert_eq!(
                    serde_json::to_value(&storage.load()?[0])?,
                    serde_json::to_value(expected)?
                );
                assert_eq!(storage.load()?[0].id, before.id);
            } else {
                assert!(result.is_err(), "{change}");
                assert_eq!(std::fs::read(&path)?, original_journal, "{change}");
                assert_eq!(
                    std::fs::read(storage.sessions_path())?,
                    original_store,
                    "{change}"
                );
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn abandoned_recovery_is_idempotent_but_rejects_changed_generation() -> Result<()> {
        for superseded in [false, true] {
            let _app = crate::session::test_support::isolate_app_dir();
            let (storage, _, admitted) = pending_fixture()?;
            let path = admitted.transaction.journal().path.clone();
            let mut entry = admitted.transaction.journal().read()?;
            let MoveAuthority::Resource { intent } = &mut entry.authority else {
                unreachable!()
            };
            intent.abandoned = true;
            admitted.transaction.journal().rewrite(&entry)?;
            let mut rows = storage.load()?;
            rows[0].lifecycle_reservation = None;
            if superseded {
                rows[0].lifecycle_generation += 1;
            }
            std::fs::write(storage.sessions_path(), serde_json::to_vec_pretty(&rows)?)?;
            let before = std::fs::read(storage.sessions_path())?;
            drop(admitted);
            let notices =
                Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())?;
            assert_eq!(path.exists(), superseded);
            assert_eq!(!notices.is_empty(), superseded);
            assert_eq!(std::fs::read(storage.sessions_path())?, before);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn resolution_rejects_symlinks_and_paths_outside_the_journal() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let directory = move_journal::journal_dir()?;
        std::fs::create_dir_all(&directory)?;
        let external = tempfile::tempdir()?;
        let target = external.path().join("foreign.json");
        std::fs::write(&target, b"{original")?;
        let alias = directory.join("alias.json");
        std::os::unix::fs::symlink(&target, &alias)?;
        for path in [&target, &alias] {
            assert!(run("", clear(path, true)).is_err());
        }
        assert_eq!(std::fs::read(target)?, b"{original");
        std::fs::remove_file(alias)?;
        let storage = Storage::new_unwatched("owner")?;
        storage.update(|rows, _| {
            rows.push(Instance::new("kept", "/kept"));
            Ok(())
        })?;
        let store_before = std::fs::read(storage.sessions_path())?;
        let alias_profile = Storage::new_unwatched("alias-profile")?;
        std::os::unix::fs::symlink(
            storage.sessions_path().parent().unwrap(),
            alias_profile
                .sessions_path()
                .parent()
                .unwrap()
                .join(".move-journal"),
        )?;
        assert!(run("owner", clear(storage.sessions_path(), true)).is_err());
        assert_eq!(std::fs::read(storage.sessions_path())?, store_before);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn cleanup_recovery_requires_completed_effect_evidence_or_attested_abandonment() -> Result<()> {
        use move_journal::{EffectPhase, EffectRecord, JournalScope, ResourceEffect, ResourceKey};
        for case in [
            "prepared",
            "applied",
            "pending",
            "missing-after",
            "bad-after",
            "abandoned",
        ] {
            let app = crate::session::test_support::isolate_app_dir();
            let storage = Storage::new_unwatched("owner")?;
            let repository = app.path().join("repository");
            git2::Repository::init(&repository)?;
            let path = app.path().join("cleanup-candidate");
            if matches!(case, "pending" | "bad-after" | "abandoned") {
                std::fs::create_dir(&path)?;
            }
            let effect = ResourceEffect::RemoveWorktree {
                repository,
                path: path.clone(),
                force: true,
            };
            let observed = effect.observe()?;
            let phase = match case {
                "prepared" => EffectPhase::Prepared,
                "pending" | "abandoned" => EffectPhase::Pending,
                _ => EffectPhase::Applied,
            };
            let authority = move_journal::ProfileAuthority::capture(&storage)?;
            let entry = MoveJournalEntry {
                version: move_journal::MOVE_JOURNAL_VERSION,
                ids: Vec::new(),
                source_profile: "owner".into(),
                target_profile: "owner".into(),
                source_sessions_path: authority.sessions_path.clone(),
                target_sessions_path: authority.sessions_path.clone(),
                group_move_source_path: String::new(),
                group_move_target_path: String::new(),
                group_move_subtree: false,
                created_at_epoch_ms: 0,
                authority: MoveAuthority::Cleanup {
                    scope: JournalScope {
                        profiles: vec![authority],
                        resources: vec![ResourceKey::path(&path, true)?],
                    },
                    effects: vec![
                        EffectRecord {
                            effect: effect.clone(),
                            phase,
                            before: (phase != EffectPhase::Prepared).then(|| observed.clone()),
                            after: matches!(case, "applied" | "bad-after").then_some(observed),
                            error: None,
                            compensation: false,
                            retained: false,
                        },
                        EffectRecord {
                            effect,
                            phase: EffectPhase::Prepared,
                            before: None,
                            after: None,
                            error: None,
                            compensation: false,
                            retained: false,
                        },
                    ],
                    abandoned: case == "abandoned",
                    conflict: None,
                },
            };
            let journal = move_journal::record(&entry)?;
            let rows = storage.load()?;
            storage::reconcile_profile_duplicates(
                &[("owner", rows.as_slice())],
                &[("owner", &storage)],
            );
            assert!(
                journal.exists(),
                "rowless cleanup is not metadata move arbitration"
            );
            let notices =
                Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())?;
            let retained = matches!(case, "pending" | "missing-after" | "bad-after");
            assert_eq!(journal.exists(), retained, "{case}");
            assert_eq!(!notices.is_empty(), retained, "{case}");
            if retained {
                assert!(crate::session::rename_profile("owner", "renamed-owner").is_err());
                run("owner", clear(&journal, true))?;
                assert!(!journal.exists());
            }
            assert_eq!(
                path.exists(),
                matches!(case, "pending" | "bad-after" | "abandoned"),
                "{case}"
            );
        }
        Ok(())
    }

    fn clear(path: &Path, actors_stopped: bool) -> IntentCommands {
        IntentCommands::Clear {
            path: Some(path.to_owned()),
            orphan_binding: None,
            actors_stopped,
            reason: None,
        }
    }

    #[test]
    #[serial_test::serial]
    fn raw_resolution_requires_attestation_and_preserves_original_bytes() -> Result<()> {
        let _app = crate::session::test_support::isolate_app_dir();
        let directory = move_journal::journal_dir()?;
        std::fs::create_dir_all(&directory)?;
        let path = directory.join("damaged.json");
        let original = b"{invalid original evidence\xff";
        std::fs::write(&path, original)?;
        assert!(run("", clear(&path, false)).is_err());
        assert_eq!(std::fs::read(&path)?, original);
        assert_eq!(
            move_journal::lease_state(&path)?,
            move_journal::LeaseState::Missing
        );
        run("", clear(&path, true))?;
        assert!(!path.exists());
        let backup = std::fs::read_dir(&directory)?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .contains(".pre-recovery-")
            })
            .context("missing backup")?;
        assert_eq!(std::fs::read(backup.path())?, original);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn orphan_resolution_is_exact_and_refuses_existing_or_live_journal() -> Result<()> {
        use fs2::FileExt;
        let _app = crate::session::test_support::isolate_app_dir();
        let storage = Storage::new_unwatched("owner")?;
        let mut original = Instance::new("owner", "/unchanged");
        let peer = Instance::new("peer", "/peer");
        storage.update(|rows, _| {
            rows.extend([original.clone(), peer.clone()]);
            Ok(())
        })?;
        let generation = storage.update(|rows, _| {
            let row = rows.iter_mut().find(|row| row.id == original.id).unwrap();
            row.try_acquire_lifecycle_reservation(
                crate::session::LifecycleOperation::Trash,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .map_err(anyhow::Error::new)
        })?;
        original = storage
            .load()?
            .into_iter()
            .find(|row| row.id == original.id)
            .unwrap();
        let root = storage::acquire_ownership_lock()?;
        let mut intent = move_journal::ResourceIntent::new(
            move_journal::IntentOwner {
                profile: move_journal::ProfileAuthority::capture(&storage)?,
                id: original.id.clone(),
                created_at: original.created_at,
                generation,
            },
            move_journal::ProfileAuthority::capture(&storage)?,
            crate::session::LifecycleOperation::Trash,
            Some(original.clone()),
            Some(original.clone()),
            Vec::new(),
            Vec::new(),
        )?;
        intent.effects.clear();
        let admitted = crate::session::claim::ResourceReservation::admit_with_ownership(
            &storage,
            &root,
            intent,
            original.clone(),
        )?;
        drop(root);
        let entry_path = move_journal::scan().entries[0].0.clone();
        let bound = storage
            .load()?
            .into_iter()
            .find(|row| row.id == original.id)
            .unwrap();
        let root = storage::acquire_ownership_lock()?;
        assert!(storage
            .release_attested_binding_with_ownership(&root, &bound)
            .is_err());
        std::fs::remove_file(&entry_path)?;
        assert!(storage
            .release_attested_binding_with_ownership(&root, &bound)
            .is_err());
        std::fs::write(&entry_path, b"{damaged current evidence")?;
        drop(admitted);
        drop(root);
        run("owner", clear(&entry_path, true))?;
        assert_eq!(
            storage.load()?[0].lifecycle_reservation,
            bound.lifecycle_reservation
        );
        let root = storage::acquire_ownership_lock()?;
        storage.release_attested_binding_with_ownership(&root, &bound)?;
        let rows = storage.load()?;
        let after = rows.iter().find(|row| row.id == original.id).unwrap();
        assert!(after.lifecycle_reservation.is_none());
        assert_eq!(after.project_path, bound.project_path);
        assert_eq!(after.lifecycle_generation, bound.lifecycle_generation);
        assert_eq!(
            serde_json::to_value(rows.iter().find(|row| row.id == peer.id).unwrap())?,
            serde_json::to_value(&peer)?
        );
        // A surviving sidecar alone must still prevent an attested release while held.
        let lease = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(entry_path.with_extension("lock"))?;
        lease.lock_exclusive()?;
        assert_eq!(
            move_journal::lease_state(&entry_path)?,
            move_journal::LeaseState::Live
        );
        Ok(())
    }
}
