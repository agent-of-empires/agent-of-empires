//! Persists the pre-v035 launch record's passphrase policy (and the rollback
//! credential derived from it).
//!
//! The daemon lifecycle transaction is only taken when there is still
//! something to write. A daemon transition holds that lock for the whole of
//! its startup wait, and it writes the launch record in the current schema
//! first, so the records are already compliant in exactly that window and
//! the lock is never contended here. The detached daemon child receives the
//! lock from its parent and therefore never migrates; it checks the schema
//! instead (`migrations::assert_schema_current`).

use std::time::Duration;

use anyhow::{Context, Result};

/// How long to wait for a daemon transition to release the lifecycle lock
/// before giving up. A stop completes inside this budget; a start does not
/// need the lock at all, because it has already published the record.
const LOCK_WAIT: Duration = Duration::from_secs(10);

pub fn run() -> Result<()> {
    let root = crate::session::AnchoredDir::open(&crate::session::get_app_dir()?)?;
    if records_are_migrated(&root)? {
        return Ok(());
    }
    let _transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(LOCK_WAIT)?;
    migrate_records(&root)
}

/// `true` when every launch record already carries this migration's outcome,
/// so there is nothing to write and no need for exclusive access. A migrated
/// record has also had its rollback credential written, since
/// `migrate_records` persists the credential before the record itself.
fn records_are_migrated(root: &crate::session::AnchoredDir) -> Result<bool> {
    for name in ["serve.launch", "serve.rollback.launch"] {
        let Some(raw) = root.bind_file(std::ffi::OsStr::new(name))?.read()? else {
            continue;
        };
        let launch: serde_json::Value = serde_json::from_str(&raw)?;
        let object = launch.as_object().context("Invalid daemon launch record")?;
        if !is_migrated(object) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Whether one persisted launch record already carries this migration's
/// outcome. `has_passphrase` and the schema are written together, so either
/// alone would do; both are checked so a future writer cannot pass for a
/// migrated record.
fn is_migrated(object: &serde_json::Map<String, serde_json::Value>) -> bool {
    object
        .get("has_passphrase")
        .is_some_and(serde_json::Value::is_boolean)
        && object.get("schema").and_then(serde_json::Value::as_u64)
            == Some(u64::from(crate::cli::serve::SERVE_LAUNCH_SCHEMA))
}

fn migrate_records(root: &crate::session::AnchoredDir) -> Result<()> {
    let current = root
        .bind_file(std::ffi::OsStr::new("serve.launch"))?
        .read()?
        .map(|raw| serde_json::from_str::<serde_json::Value>(&raw))
        .transpose()?;
    let passphrase = root
        .bind_file(std::ffi::OsStr::new("serve.passphrase"))?
        .read()?;
    for name in ["serve.launch", "serve.rollback.launch"] {
        let file = root.bind_file(std::ffi::OsStr::new(name))?;
        let Some(raw) = file.read()? else { continue };
        let mut launch: serde_json::Value = serde_json::from_str(&raw)?;
        let object = launch
            .as_object_mut()
            .context("Invalid daemon launch record")?;
        if is_migrated(object) {
            continue;
        }
        let current_identity = current.as_ref().is_some_and(|current| {
            current
                .get("pid")
                .is_some_and(|pid| pid.is_u64() && Some(pid) == object.get("pid"))
                && current
                    .get("instance_id")
                    .and_then(|value| value.as_str())
                    .is_some_and(|instance| {
                        !instance.is_empty()
                            && object.get("instance_id").and_then(|value| value.as_str())
                                == Some(instance)
                    })
        });
        let has_passphrase = if name == "serve.launch" || current_identity {
            passphrase.is_some()
                || object.get("remote").and_then(|value| value.as_bool()) == Some(true)
                || object.get("auth_mode").and_then(|value| value.as_str()) == Some("passphrase")
        } else {
            // An unrelated token launch may have had a secondary login gate.
            object.get("core_only").and_then(|value| value.as_bool()) != Some(true)
                && object.get("auth_mode").and_then(|value| value.as_str()) != Some("none")
        };
        object.insert("has_passphrase".into(), has_passphrase.into());
        object.insert(
            "schema".into(),
            u64::from(crate::cli::serve::SERVE_LAUNCH_SCHEMA).into(),
        );
        if name == "serve.rollback.launch" && (!has_passphrase || current_identity) {
            let credential = serde_json::json!({
                "pid": object.get("pid"),
                "instance_id": object.get("instance_id"),
                "passphrase": if has_passphrase { passphrase.as_deref() } else { None },
            });
            root.bind_file(std::ffi::OsStr::new("serve.rollback.passphrase"))?
                .replace(&serde_json::to_vec(&credential)?)?;
        }
        file.replace(&serde_json::to_vec_pretty(&launch)?)?;
        tracing::info!(name, "Migrated persisted daemon passphrase policy");
    }
    Ok(())
}
