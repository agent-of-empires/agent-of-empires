//! Preserve legacy SID exclusions without inventing their namespace.

use super::progress;
use anyhow::{Context, Result};
use serde_json::Value;
use std::{fs, path::Path};

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let profiles = app_dir.join("profiles");
    if profiles.exists() {
        for entry in fs::read_dir(profiles)? {
            let entry = entry?;
            if entry.path().is_dir() {
                migrate_file(&entry.path().join("sessions.json"))?;
            }
        }
    }
    migrate_file(&app_dir.join("sessions.json"))
}

fn migrate_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let directory = path.parent().context("sessions path has no parent")?;
    let _lock =
        crate::session::acquire_storage_flock(directory, crate::session::STORAGE_LOCK_FILENAME)?;
    let mut value: Value = match serde_json::from_str(&fs::read_to_string(path)?) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!("v032: cannot parse {}: {error}; skipping", path.display());
            return Ok(());
        }
    };
    let mut changed = false;
    if let Some(instances) = value.as_array_mut() {
        for instance in instances {
            if let Some(exclusions) = instance
                .get_mut("retroactive_capture_excludes")
                .and_then(Value::as_array_mut)
            {
                for exclusion in exclusions {
                    if let Some(sid) = exclusion.as_str() {
                        *exclusion = serde_json::to_value(
                            crate::session::ConversationBinding::unknown(sid),
                        )?;
                        changed = true;
                    }
                }
            }
        }
    }
    if changed {
        if let Err(error) = crate::session::backup_before_migration(path) {
            tracing::warn!(%error, path = %path.display(), "v032: no migration backup for the retype");
            progress::notice("could not back up sessions.json before binding capture exclusions");
        }
        crate::session::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())?;
        tracing::info!(
            "v032: bound legacy capture exclusions in {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_exclusions_remain_conservative_after_upgrade_and_reentry() {
        let temp = tempfile::tempdir().unwrap();
        let profile = temp.path().join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        let path = profile.join("sessions.json");
        fs::write(
            &path,
            r#"[{"retroactive_capture_excludes":["legacy-sid"]}]"#,
        )
        .unwrap();
        run_in(temp.path()).unwrap();
        run_in(temp.path()).unwrap();
        let rows: Value = serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let exclusions: Vec<crate::session::ConversationBinding> =
            serde_json::from_value(rows[0]["retroactive_capture_excludes"].clone()).unwrap();
        let source = crate::session::ExecutionBinding {
            agent: "claude".into(),
            stores: vec![temp.path().join("new-store")],
            configuration: Vec::new(),
            exported_default_store: false,
            cwd: temp.path().into(),
            cwd_filesystem: "host".into(),
            filesystem: "host".into(),
        };
        assert!(exclusions
            .iter()
            .any(|binding| binding.excludes_capture("legacy-sid", Some(&source))));
        assert!(!exclusions
            .iter()
            .any(|binding| binding.excludes_capture("new-sid", Some(&source))));
    }

    #[test]
    fn retyping_leaves_a_migration_backup_the_previous_release_can_read() {
        // v1.16.1 typed this field as `HashSet<String>`.
        #[derive(serde::Deserialize)]
        struct Pre116 {
            #[serde(default)]
            retroactive_capture_excludes: std::collections::HashSet<String>,
        }

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("profiles/default/sessions.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            r#"[{"retroactive_capture_excludes":["legacy-sid"]}]"#,
        )
        .unwrap();

        run_in(temp.path()).unwrap();
        run_in(temp.path()).unwrap();

        let backups = crate::session::migration_backups(&path).unwrap();
        assert_eq!(
            backups.len(),
            1,
            "re-entry must not add another migration backup: {backups:?}"
        );
        let before: Vec<Pre116> =
            serde_json::from_slice(&fs::read(&backups[0].1).unwrap()).unwrap();
        assert!(before[0]
            .retroactive_capture_excludes
            .contains("legacy-sid"));
    }
}
