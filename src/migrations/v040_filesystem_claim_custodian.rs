//! Preserve scoped filesystem uncertainty when the original custodian is unrecoverable.

use anyhow::{Context, Result};
use std::fs;

pub fn run() -> Result<()> {
    tracing::info!(target: "migrations", "v040: retaining crash claim inventories and custodian births");
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let _namespace = crate::session::acquire_profile_namespace_lock()?;
    let app = crate::session::get_app_dir()?;
    let mut paths = crate::session::list_profiles_for_worktree_inventory()?
        .into_iter()
        .map(|profile| app.join("profiles").join(profile).join("sessions.json"))
        .collect::<Vec<_>>();
    paths.push(app.join("sessions.json"));
    for path in paths {
        let directory = path.parent().context("sessions file has no parent")?;
        let original = fs::File::open(directory)?;
        let _lock = crate::session::acquire_storage_flock(
            directory,
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        anyhow::ensure!(
            crate::session::same_filesystem_identity(
                &original.metadata()?,
                &fs::metadata(directory)?
            ),
            "profile changed during custodian migration"
        );
        let content = match fs::read_to_string(&path) {
            Ok(content) if content.trim().is_empty() => continue,
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(&path) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    _ => anyhow::bail!("unreadable sessions file {}", path.display()),
                }
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        let mut document = crate::session::raw_document::RawDocument::parse(&content)
            .with_context(|| format!("parsing {}", path.display()))?;
        let profile = crate::session::DirectoryIdentity::from_metadata(&original.metadata()?);
        if migrate_document(&mut document, profile)? {
            crate::session::backup_before_migration(&path)?;
            anyhow::ensure!(
                crate::session::same_filesystem_identity(
                    &original.metadata()?,
                    &fs::metadata(directory)?
                ),
                "profile changed before custodian migration commit"
            );
            crate::session::atomic_write(&path, &serde_json::to_vec_pretty(&document.rows)?)?;
            crate::session::sync_parent_directory(&path)?;
        }
    }
    Ok(())
}

fn migrate_document(
    document: &mut crate::session::raw_document::RawDocument,
    profile: crate::session::DirectoryIdentity,
) -> Result<bool> {
    use crate::session::raw_document::{patch, Emission, RawObject};
    let owners = document.owners("id");
    let mut changed = false;
    for row in &mut document.rows {
        let Ok(fields) = RawObject::parse(row) else {
            continue;
        };
        let Ok(Some(id)) = fields.unique("id") else {
            continue;
        };
        let Ok(id) = serde_json::from_str::<String>(id.get()) else {
            continue;
        };
        if !owners
            .get(&id)
            .is_some_and(|owner| owner.count == 1 && !owner.ambiguous)
        {
            continue;
        }
        let Ok(Some(lease)) = fields.unique("lifecycle_reservation") else {
            continue;
        };
        let Ok(lease) = RawObject::parse(lease) else {
            continue;
        };
        let Ok(Some(operation)) = lease.unique("op") else {
            continue;
        };
        if serde_json::from_str::<crate::session::LifecycleOperation>(operation.get()).is_err() {
            continue;
        }
        let Ok(custodian) = lease.unique("custodian") else {
            continue;
        };
        if custodian.is_some_and(|value| {
            serde_json::from_str::<Option<crate::process::OriginalCustodianBirth>>(value.get())
                .is_err()
        }) {
            continue;
        }
        let Ok(Some(claims)) = lease.unique("path_claims") else {
            continue;
        };
        let Ok(claims) = RawObject::parse(claims) else {
            continue;
        };
        let Ok(Some(state)) = claims.unique("state") else {
            continue;
        };
        let Ok(state) = serde_json::from_str::<String>(state.get()) else {
            continue;
        };
        let Ok(paths) = claims.unique("paths") else {
            continue;
        };
        let mut before = serde_json::json!({"lifecycle_reservation": {}});
        let mut after = before.clone();
        match state.as_str() {
            "pending" => {
                let Some(paths) = paths else { continue };
                if serde_json::from_str::<Vec<std::path::PathBuf>>(paths.get()).is_err() {
                    continue;
                }
                let evidence = crate::session::claim_reconcile::pending_custodian(row, profile);
                if !matches!(evidence, Ok(Some(_))) {
                    before["lifecycle_reservation"]["path_claims"] =
                        serde_json::json!({"state": "pending"});
                    after["lifecycle_reservation"]["path_claims"] =
                        serde_json::json!({"state": "unknown"});
                }
            }
            "unknown" => {
                if paths.is_some_and(|paths| {
                    serde_json::from_str::<Option<Vec<std::path::PathBuf>>>(paths.get()).is_err()
                }) {
                    continue;
                }
                if paths.is_none() {
                    before["lifecycle_reservation"]["path_claims"] =
                        serde_json::json!({"state": "unknown"});
                    after["lifecycle_reservation"]["path_claims"] =
                        serde_json::json!({"state": "unknown", "paths": null});
                }
            }
            "none" => {}
            _ => continue,
        }
        if custodian.is_none() {
            after["lifecycle_reservation"]["custodian"] = serde_json::Value::Null;
        }
        if let Emission::Changed(updated) = patch(row, &before, &after)? {
            *row = updated;
            changed = true;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::raw_document::{RawDocument, RawObject};

    #[test]
    fn handcrafted_legacy_claims_keep_literals_and_ambiguous_rows_losslessly() -> Result<()> {
        let profile = crate::session::DirectoryIdentity {
            device: 1,
            inode: 2,
            birth_time: Some(std::time::UNIX_EPOCH),
        };
        let extension = r#"{"same":1,"same":2,"huge":1234567890123456789012345678901234567890,"float":1e400,"escaped":"\u0061"}"#;
        let paths = r#"["/tmp/a","/tmp/\u0062","/tmp/a"]"#;
        let pending = format!(
            r#"{{"id":"pending","created_at":"2000-01-01T00:00:00Z","lifecycle_generation":4,"lifecycle_reservation":{{"op":"create","generation":4,"at":"2000-01-01T00:00:00Z","path_claims":{{"state":"pending","paths":{paths},"extension":{extension}}},"extension":{extension}}},"extension":{extension}}}"#
        );
        let unknown = r#"{"id":"historical","lifecycle_reservation":{"op":"attach","generation":4,"path_claims":{"state":"unknown"}}}"#;
        let none = r#"{"id":"ordinary","lifecycle_reservation":{"op":"stop","path_claims":{"state":"none"}}}"#;
        let duplicate = r#"{"id":"duplicated","lifecycle_reservation":{"op":"create","path_claims":{"state":"pending","paths":["/tmp/keep"]}}}"#;
        let corrupt = r#"{"id":"corrupt","lifecycle_reservation":{"op":"create","path_claims":{"state":"pending","state":"unknown","paths":["/tmp/keep"]}}}"#;
        let mut document = RawDocument::parse(&format!(
            "[{pending},{unknown},{none},{duplicate},{duplicate},{corrupt},17]"
        ))?;
        assert!(migrate_document(&mut document, profile)?);
        let fields = RawObject::parse(&document.rows[0])?;
        assert_eq!(fields.unique("extension")?.unwrap().get(), extension);
        let lease = RawObject::parse(fields.unique("lifecycle_reservation")?.unwrap())?;
        assert_eq!(lease.unique("extension")?.unwrap().get(), extension);
        assert_eq!(lease.unique("custodian")?.unwrap().get(), "null");
        let claims = RawObject::parse(lease.unique("path_claims")?.unwrap())?;
        assert_eq!(claims.unique("state")?.unwrap().get(), r#""unknown""#);
        assert_eq!(claims.unique("paths")?.unwrap().get(), paths);
        assert_eq!(claims.unique("extension")?.unwrap().get(), extension);
        let claims: crate::session::WorktreePathClaims = serde_json::from_str(
            RawObject::parse(
                RawObject::parse(&document.rows[1])?
                    .unique("lifecycle_reservation")?
                    .unwrap(),
            )?
            .unique("path_claims")?
            .unwrap()
            .get(),
        )?;
        assert_eq!(claims, crate::session::WorktreePathClaims::Unknown(None));
        assert_eq!(document.rows[3].get(), duplicate);
        assert_eq!(document.rows[4].get(), duplicate);
        assert_eq!(document.rows[5].get(), corrupt);
        assert_eq!(document.rows[6].get(), "17");
        let once = serde_json::to_vec(&document.rows)?;
        assert!(!migrate_document(&mut document, profile)?);
        assert_eq!(serde_json::to_vec(&document.rows)?, once);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn v040_backs_up_handcrafted_legacy_profiles_and_is_idempotent() -> Result<()> {
        let _home = crate::session::test_support::isolate_app_dir();
        let storage = crate::session::Storage::new_unwatched("legacy-claims")?;
        let root = crate::session::get_app_dir()?.join("sessions.json");
        let named = r#"[{"id":"pending","project_path":"/tmp/current","lifecycle_reservation":{"op":"create","generation":7,"at":"2000-01-01T00:00:00Z","path_claims":{"state":"pending","paths":["/tmp/a","/tmp/\u0062","/tmp/a"],"opaque":{"same":1,"same":2,"big":1e400}}}}]"#;
        let legacy = r#"[{"id":"historical","project_path":"/tmp/current","lifecycle_reservation":{"op":"attach","generation":4,"at":"2000-01-01T00:00:00Z","path_claims":{"state":"unknown"}}}]"#;
        fs::write(storage.sessions_path(), named)?;
        fs::write(&root, legacy)?;
        run()?;
        let named_backups = crate::session::migration_backups(storage.sessions_path())?;
        let root_backups = crate::session::migration_backups(&root)?;
        assert_eq!(named_backups.len(), 1);
        assert_eq!(root_backups.len(), 1);
        assert_eq!(fs::read(&named_backups[0].1)?, named.as_bytes());
        assert_eq!(fs::read(&root_backups[0].1)?, legacy.as_bytes());
        let migrated = fs::read_to_string(storage.sessions_path())?;
        let document = RawDocument::parse(&migrated)?;
        let row = RawObject::parse(&document.rows[0])?;
        let lease = RawObject::parse(row.unique("lifecycle_reservation")?.unwrap())?;
        assert_eq!(lease.unique("generation")?.unwrap().get(), "7");
        let claims = RawObject::parse(lease.unique("path_claims")?.unwrap())?;
        assert_eq!(claims.unique("state")?.unwrap().get(), r#""unknown""#);
        assert_eq!(
            claims.unique("paths")?.unwrap().get(),
            r#"["/tmp/a","/tmp/\u0062","/tmp/a"]"#
        );
        assert_eq!(
            claims.unique("opaque")?.unwrap().get(),
            r#"{"same":1,"same":2,"big":1e400}"#
        );
        let root_after = fs::read(&root)?;
        run()?;
        assert_eq!(fs::read_to_string(storage.sessions_path())?, migrated);
        assert_eq!(fs::read(&root)?, root_after);
        assert_eq!(
            crate::session::migration_backups(storage.sessions_path())?.len(),
            1
        );
        assert_eq!(crate::session::migration_backups(&root)?.len(), 1);
        Ok(())
    }
}
