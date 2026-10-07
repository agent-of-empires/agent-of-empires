//! Legacy journals and registry tuples cannot retrospectively prove native birth custody.

use anyhow::{Context, Result};
use std::fs;

pub fn run() -> Result<()> {
    tracing::info!(target: "migrations", "v037: fencing unknown legacy preparation coverage");
    let _workspace = crate::session::acquire_session_workspace_claim_lock()?;
    let _identity = crate::session::acquire_session_identity_lock()?;
    let unknown =
        serde_json::to_value(crate::session::runner_journal::RunnerExecutionJournal::default())?;
    for path in super::sessions_file::session_files(&crate::session::get_app_dir()?)? {
        let directory = path.parent().context("sessions file has no parent")?;
        let origin = fs::metadata(directory)?;
        let _lock = crate::session::acquire_storage_flock(
            directory,
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        anyhow::ensure!(
            crate::session::same_filesystem_identity(&origin, &fs::metadata(directory)?),
            "profile was replaced before preparation coverage migration"
        );
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        super::sessions_file::heal_rows(&path, &content, |row| {
            let Some(journal) = row
                .get_mut("runner_journal")
                .and_then(|value| value.as_object_mut())
            else {
                if row
                    .get("runner_journal")
                    .is_none_or(|value| value.is_null())
                {
                    row.insert("runner_journal".into(), unknown.clone());
                    return true;
                }
                return false;
            };
            let mut changed = false;
            let missing_birth = journal
                .get("launches")
                .and_then(|value| value.as_array())
                .is_some_and(|launches| {
                    launches.iter().any(|launch| {
                        launch.as_object().is_some_and(|launch| {
                            launch
                                .get("profile_identity")
                                .and_then(|identity| identity.get("birth_time"))
                                .is_none_or(|birth| birth.is_null())
                        })
                    })
                });
            if missing_birth {
                changed |= journal.get("coverage") != Some(&unknown["coverage"])
                    || journal.get("boot") != Some(&unknown["boot"]);
                journal.insert("coverage".into(), unknown["coverage"].clone());
                journal.insert("boot".into(), unknown["boot"].clone());
            }
            if !journal.contains_key("preparations") {
                journal.insert("coverage".into(), unknown["coverage"].clone());
                journal.insert("boot".into(), unknown["boot"].clone());
                journal.insert("preparations".into(), serde_json::json!([]));
                changed = true;
            }
            for launch in journal
                .get_mut("launches")
                .and_then(|value| value.as_array_mut())
                .into_iter()
                .flatten()
            {
                if let Some(launch) = launch.as_object_mut() {
                    if !launch.contains_key("profile_identity") {
                        launch.insert("profile_identity".into(), serde_json::Value::Null);
                        changed = true;
                    }
                    if !launch.contains_key("stop_endpoint") {
                        launch.insert("stop_endpoint".into(), serde_json::Value::Null);
                        changed = true;
                    }
                    if !launch.contains_key("registry") {
                        launch.insert("registry".into(), serde_json::Value::Null);
                        changed = true;
                    }
                    for field in ["profile_identity", "stop_endpoint"] {
                        if let Some(value) = launch.get_mut(field) {
                            changed |= quarantine_weak_birth(value);
                        }
                    }
                }
            }
            changed
        })?;
        crate::session::sync_parent_directory(&path)?;
    }
    crate::process::worker_registry::migrate_birth_stamps(|record| {
        let mut changed = false;
        for field in ["profile_identity", "control_file_identity"] {
            if let Some(value) = record.get_mut(field) {
                changed |= quarantine_weak_birth(value);
            }
        }
        changed
    })?;
    Ok(())
}

fn quarantine_weak_birth(value: &mut serde_json::Value) -> bool {
    if let Some(tuple) = value.as_array().filter(|tuple| tuple.len() == 2) {
        if let Some((device, inode)) = tuple[0].as_u64().zip(tuple[1].as_u64()) {
            *value = serde_json::json!({ "device": device, "inode": inode, "birth_time": null });
            return true;
        }
    }
    if let Some(stamp) = value.as_object_mut() {
        if stamp.contains_key("device")
            && stamp.contains_key("inode")
            && !stamp.contains_key("birth_time")
        {
            stamp.insert("birth_time".into(), serde_json::Value::Null);
            return true;
        }
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn applied_v036_is_migrated_without_retroactive_preparation_proof() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let app = crate::session::get_app_dir().unwrap();
        let directory = app.join("profiles/default");
        fs::create_dir_all(&directory).unwrap();
        fs::write(app.join(".schema_version"), b"36").unwrap();
        let boot = crate::session::runner_journal::current_boot().unwrap();
        let incarnation = crate::process::process_incarnation(std::process::id())
            .unwrap()
            .unwrap();
        let historical_nonce = [1u8; 16];
        let preparation_nonce = [2u8; 16];
        let history = serde_json::json!({
            "nonce": historical_nonce, "boot": boot, "generation": 17,
            "incarnation": incarnation, "sentinel": "retain"
        });
        let pending = serde_json::json!({
            "coverage": "complete", "launches": [],
            "preparations": [{"nonce": preparation_nonce, "boot": boot, "generation": 19}]
        });
        let mut legacy =
            serde_json::to_value(crate::session::Instance::new("legacy", "/tmp/native")).unwrap();
        legacy["runner_journal"] =
            serde_json::json!({"coverage": "complete", "launches": [history.clone()]});
        let mut preparing =
            serde_json::to_value(crate::session::Instance::new("preparing", "/tmp/native"))
                .unwrap();
        preparing["runner_journal"] = pending.clone();
        let path = directory.join("sessions.json");
        fs::write(&path, serde_json::to_vec(&vec![legacy, preparing]).unwrap()).unwrap();

        crate::migrations::run_migrations().unwrap();

        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let journal = &migrated[0]["runner_journal"];
        assert_eq!(journal["coverage"], "unknown");
        assert_eq!(journal["boot"], serde_json::json!(boot));
        assert_eq!(journal["preparations"], serde_json::json!([]));
        let migrated_history = journal["launches"].as_array().unwrap();
        assert_eq!(migrated_history.len(), 1);
        for (key, value) in history.as_object().unwrap() {
            assert_eq!(
                &migrated_history[0][key], value,
                "historical birth data must survive migration"
            );
        }
        assert_eq!(migrated[1]["runner_journal"], pending);
        let loaded: crate::session::runner_journal::RunnerExecutionJournal =
            serde_json::from_value(journal.clone()).unwrap();
        assert!(!loaded.proves_quiescent());
        let bytes = fs::read(&path).unwrap();
        crate::migrations::run_migrations().unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }

    #[test]
    #[serial_test::serial]
    fn weak_inode_tuples_never_acquire_birth_time_from_the_current_namespace() {
        let temporary = tempfile::tempdir().unwrap();
        let _environment = crate::session::test_support::isolate_app_dir_at(temporary.path());
        let app = crate::session::get_app_dir().unwrap();
        let profile = app.join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        fs::write(app.join(".schema_version"), b"36").unwrap();
        let instance = crate::session::Instance::new("weak", "/tmp/native");
        let mut row = serde_json::to_value(&instance).unwrap();
        row["runner_journal"] = serde_json::json!({
            "coverage": "complete", "preparations": [],
            "launches": [{"nonce": ([0u8; 16]), "boot": crate::session::runner_journal::current_boot().unwrap(),
                "generation": 0, "profile_identity": [17, 19],
                "stop_endpoint": {"device": 23, "inode": 29}}]
        });
        fs::write(
            profile.join("sessions.json"),
            serde_json::to_vec(&vec![row]).unwrap(),
        )
        .unwrap();
        let workers = app.join("acp-workers");
        fs::create_dir_all(&workers).unwrap();
        let record = crate::process::worker_registry::WorkerRecord::new(
            instance.id.clone(),
            std::process::id(),
            workers.join("weak.sock"),
            "fixture".into(),
            "fixture".into(),
            "/tmp/native".into(),
            None,
            vec![],
            vec![],
            None,
            Some("default".into()),
        );
        let mut record = serde_json::to_value(record).unwrap();
        record["profile_identity"] = serde_json::json!([17, 19]);
        record["control_file_identity"] = serde_json::json!([31, 37]);
        fs::write(
            workers.join(format!("{}.json", instance.id)),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();

        crate::migrations::run_migrations().unwrap();
        let storage = crate::session::Storage::open_unwatched("default").unwrap();
        let loaded = storage.load().unwrap().remove(0);
        assert!(!loaded.runner_journal.proves_quiescent());
        let record = crate::process::worker_registry::load_strict(&instance.id)
            .unwrap()
            .unwrap();
        let profile_identity = record.profile_identity.unwrap();
        assert_eq!((profile_identity.device, profile_identity.inode), (17, 19));
        assert!(!profile_identity.is_durable());
        let control_identity = record.control_file_identity.unwrap();
        assert_eq!((control_identity.device, control_identity.inode), (31, 37));
        assert!(!control_identity.is_durable());
        let bytes: serde_json::Value =
            serde_json::from_slice(&fs::read(storage.sessions_path()).unwrap()).unwrap();
        let launch = &bytes[0]["runner_journal"]["launches"][0];
        assert_eq!(
            launch["profile_identity"]["birth_time"],
            serde_json::Value::Null
        );
        assert_eq!(
            launch["stop_endpoint"]["birth_time"],
            serde_json::Value::Null
        );
    }
}
