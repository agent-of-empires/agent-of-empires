//! Reconciles draft launch-policy records without dropping lifecycle authority.
//! Startup borrows its held transaction; standalone migration acquires one.
//! Detached daemon children only assert the completed schema.

use std::time::Duration;

use anyhow::{Context, Result};

/// How long to wait for a daemon transition to release the lifecycle lock
/// before giving up. A stop completes inside this budget; a start does not
/// need the lock at all, because it has already published the record.
const LOCK_WAIT: Duration = Duration::from_secs(10);

pub fn run() -> Result<()> {
    let transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(LOCK_WAIT)?;
    run_with_exclusion(&transaction, None)
}

pub(super) fn run_with_transaction(
    transaction: &crate::daemon::lifecycle::Transaction,
) -> Result<()> {
    run_with_exclusion(transaction, None)
}

pub(super) fn run_with_exclusion(
    transaction: &crate::daemon::lifecycle::Transaction,
    serve: Option<&crate::cli::serve::ServeLock>,
) -> Result<()> {
    let root = crate::session::AnchoredDir::open(&crate::session::get_app_dir()?)?;
    let malformed = malformed_records(&root)?;
    if malformed.is_empty() {
        let _state = crate::cli::serve::lock_serve_state()?;
        return migrate_records(&root);
    }
    crate::cli::serve::with_inactive_serve_state(transaction, serve, |_| {
        // Revalidate both slots before preserving or removing either one.
        let malformed = malformed_records(&root)?;
        for (name, bytes) in malformed {
            let file = root.bind_file(name.as_ref())?;
            file.require_writable_existing()?;
            let quarantine = file.quarantine_recovery_bytes(&bytes)?;
            file.remove()?;
            file.sync_parent()?;
            tracing::warn!(record = name, path = %quarantine.display(), "Preserved malformed inactive launch state; start a fresh daemon explicitly");
        }
        migrate_records(&root)
    })
}

fn malformed_records(root: &crate::session::AnchoredDir) -> Result<Vec<(&'static str, Vec<u8>)>> {
    let mut malformed = Vec::new();
    for name in ["serve.launch", "serve.rollback.launch"] {
        let file = root.bind_file(name.as_ref())?;
        let Some(bytes) = file
            .read_recovery_bytes()
            .with_context(|| format!("Reading {name}"))?
        else {
            continue;
        };
        let parsed = serde_json::from_slice::<serde_json::Value>(&bytes);
        match parsed {
            Ok(serde_json::Value::Object(object)) => {
                validate_record_schema(&object).with_context(|| format!("Validating {name}"))?;
            }
            Ok(_) | Err(_) => malformed.push((name, bytes)),
        }
    }
    Ok(malformed)
}

#[derive(Debug, thiserror::Error)]
#[error("Unsupported daemon launch schema: {0:?}")]
struct UnsupportedLaunchSchema(Option<u64>);

fn validate_record_schema(object: &serde_json::Map<String, serde_json::Value>) -> Result<()> {
    let schema = object.get("schema").and_then(serde_json::Value::as_u64);
    let supported = u64::from(crate::cli::serve::SERVE_LAUNCH_SCHEMA);
    anyhow::ensure!(
        schema.is_some_and(|schema| (1..=supported).contains(&schema)),
        UnsupportedLaunchSchema(schema)
    );
    if schema == Some(supported) {
        anyhow::ensure!(
            is_migrated(object),
            "Invalid current daemon passphrase policy"
        );
    }
    Ok(())
}

/// `true` when every launch record already carries this migration's outcome,
/// so there is nothing to write and no need for exclusive access. A migrated
/// record has also had its rollback credential written, since
/// `migrate_records` persists the credential before the record itself.
fn records_are_migrated(root: &crate::session::AnchoredDir) -> Result<bool> {
    let mut migrated = true;
    for name in ["serve.launch", "serve.rollback.launch"] {
        let Some(raw) = root.bind_file(std::ffi::OsStr::new(name))?.read()? else {
            continue;
        };
        let launch: serde_json::Value = serde_json::from_str(&raw)?;
        let object = launch.as_object().context("Invalid daemon launch record")?;
        validate_record_schema(object)?;
        migrated &= is_migrated(object);
    }
    Ok(migrated)
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
    // Validate both records before rewriting either, including under held ownership.
    records_are_migrated(root)?;
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
        validate_record_schema(object)?;
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
        tracing::info!(target: "migrations.v044", name, "Migrated persisted daemon passphrase policy");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::{update_app_state, AppStateConfig, SortOrder};
    use serde_json::{json, Value};
    use std::fs;
    #[test]
    #[serial_test::serial]
    fn skipped_policy_upgrade_preserves_identity_and_login_policy() -> anyhow::Result<()> {
        for version in 35..=39 {
            for (auth, secret, protected) in [
                ("none", None, false),
                ("token", Some("secondary login"), true),
                ("passphrase", None, true),
            ] {
                let temp = tempfile::tempdir()?;
                let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
                let root = crate::session::get_app_dir()?;
                std::fs::write(root.join(".schema_version"), version.to_string())?;
                if version >= 36 {
                    crate::session::purge_owners::initialize(&root)?;
                }
                let launch = serde_json::json!({"schema":2,"pid":42,"instance_id":"retained-instance","auth_mode":auth,"remote":false});
                for name in ["serve.launch", "serve.rollback.launch"] {
                    std::fs::write(root.join(name), serde_json::to_vec(&launch)?)?;
                }
                if let Some(secret) = secret {
                    std::fs::write(root.join("serve.passphrase"), secret)?;
                }
                crate::migrations::run_migrations()?;
                for name in ["serve.launch", "serve.rollback.launch"] {
                    let migrated: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(root.join(name))?)?;
                    assert_eq!(
                        migrated["has_passphrase"], protected,
                        "version={version}, auth={auth}"
                    );
                    assert_eq!(migrated["pid"], launch["pid"]);
                    assert_eq!(migrated["instance_id"], launch["instance_id"]);
                    assert_eq!(migrated["auth_mode"], launch["auth_mode"]);
                }
                let credential: serde_json::Value = serde_json::from_slice(&std::fs::read(
                    root.join("serve.rollback.passphrase"),
                )?)?;
                assert_eq!(credential["pid"], launch["pid"]);
                assert_eq!(credential["instance_id"], launch["instance_id"]);
                assert_eq!(credential["passphrase"].as_str(), secret);
                let retained = std::fs::read(root.join("serve.rollback.passphrase"))?;
                run()?;
                assert_eq!(
                    std::fs::read(root.join("serve.rollback.passphrase"))?,
                    retained
                );
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn inactive_malformed_launch_is_preserved_and_peer_secrets_survive() -> anyhow::Result<()> {
        for torn in ["serve.launch", "serve.rollback.launch"] {
            let temp = tempfile::tempdir()?;
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir()?;
            let valid = serde_json::json!({"schema":crate::cli::serve::SERVE_LAUNCH_SCHEMA,"pid":42,"instance_id":"retained","auth_mode":"token","has_passphrase":true});
            let peer = if torn == "serve.launch" {
                "serve.rollback.launch"
            } else {
                "serve.launch"
            };
            let peer_bytes = serde_json::to_vec(&valid)?;
            let corrupt = [b'{', b'"', 0xff];
            std::fs::write(app.join(torn), corrupt)?;
            std::fs::write(app.join(peer), &peer_bytes)?;
            std::fs::write(app.join("serve.passphrase"), b"secret")?;
            std::fs::write(app.join("serve.rollback.passphrase"), b"rollback-secret")?;
            run()?;
            assert!(!app.join(torn).try_exists()?);
            assert_eq!(std::fs::read(app.join(peer))?, peer_bytes);
            assert_eq!(std::fs::read(app.join("serve.passphrase"))?, b"secret");
            assert_eq!(
                std::fs::read(app.join("serve.rollback.passphrase"))?,
                b"rollback-secret"
            );
            let quarantines: Vec<_> = std::fs::read_dir(&app)?
                .collect::<std::io::Result<Vec<_>>>()?
                .into_iter()
                .filter(|entry| {
                    entry
                        .file_name()
                        .to_string_lossy()
                        .starts_with(&format!("{torn}.quarantine-"))
                })
                .collect();
            assert_eq!(quarantines.len(), 1);
            assert_eq!(std::fs::read(quarantines[0].path())?, corrupt);
            run()?;
            assert_eq!(std::fs::read(quarantines[0].path())?, corrupt);
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn ambiguous_daemon_or_future_peer_prevents_any_quarantine() -> anyhow::Result<()> {
        for future_peer in [false, true] {
            let temp = tempfile::tempdir()?;
            let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir()?;
            std::fs::write(app.join("serve.launch"), b"{")?;
            std::fs::write(app.join("serve.passphrase"), b"retained-secret")?;
            if future_peer {
                std::fs::write(
                    app.join("serve.rollback.launch"),
                    br#"{"schema":18446744073709551615}"#,
                )?;
            } else {
                // A torn PID is not absence proof, even when lifetime locks are free.
                std::fs::write(app.join("serve.pid"), b"not-a-pid")?;
            }
            assert!(run().is_err());
            assert_eq!(std::fs::read(app.join("serve.launch"))?, b"{");
            assert_eq!(
                std::fs::read(app.join("serve.passphrase"))?,
                b"retained-secret"
            );
            assert!(!std::fs::read_dir(&app)?
                .collect::<std::io::Result<Vec<_>>>()?
                .iter()
                .any(|entry| entry.file_name().to_string_lossy().contains(".quarantine-")));
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn occupied_legacy_lifetime_refuses_malformed_launch_recovery() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir()?;
        std::fs::write(app.join("serve.launch"), b"{")?;
        let held = crate::cli::serve::try_lock_serve()?.unwrap();
        assert!(run().is_err());
        assert_eq!(std::fs::read(app.join("serve.launch"))?, b"{");
        assert!(!std::fs::read_dir(&app)?
            .collect::<std::io::Result<Vec<_>>>()?
            .iter()
            .any(|entry| entry.file_name().to_string_lossy().contains(".quarantine-")));
        drop(held);
        run()?;
        assert!(!app.join("serve.launch").try_exists()?);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn published_and_recent_pr_histories_preserve_durable_state() -> anyhow::Result<()> {
        for version in 34..=43 {
            let temp = tempfile::tempdir()?;
            let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir()?;
            fs::write(app.join(".schema_version"), version.to_string())?;
            let seeded = matches!(version, 36 | 37 | 42 | 43);
            update_app_state(|state| {
                state.sort_order = Some(SortOrder::Custom);
                state.sessions_created_seeded = seeded;
                state.sessions_created = if seeded { 19 } else { 0 };
                state.tips_seen.push("retained-tip".into());
            })?;
            let mut row = json!({"id":"retained", "trashed_at":"2026-10-11T00:00:00Z", "lifecycle_generation":7});
            if version == 37 || version == 43 {
                row["trash_lifecycle_id"] = json!("01234567-89ab-4cde-8fab-0123456789ab");
            }
            if version >= 38 {
                row["failed_purge"] =
                    json!({"trashed_at":row["trashed_at"], "lifecycle_generation":7,"forced":true});
                if version == 43 {
                    row["failed_purge"]["trash_lifecycle_id"] = row["trash_lifecycle_id"].clone();
                }
            }
            fs::write(
                app.join("sessions.json"),
                serde_json::to_vec(&[row.clone()])?,
            )?;
            let launch = if version <= 37 {
                json!({"schema":2,"pid":42,"instance_id":"retained-instance","auth_mode":"token","remote":false})
            } else {
                json!({"schema":crate::cli::serve::SERVE_LAUNCH_SCHEMA,"pid":42,"instance_id":"retained-instance","auth_mode":"token","remote":false,"has_passphrase":true})
            };
            for name in ["serve.launch", "serve.rollback.launch"] {
                fs::write(app.join(name), serde_json::to_vec(&launch)?)?;
            }
            fs::write(app.join("serve.passphrase"), "secondary login")?;
            let credential =
                json!({"pid":42,"instance_id":"retained-instance","passphrase":"secondary login"});
            if version >= 38 {
                fs::write(
                    app.join("serve.rollback.passphrase"),
                    serde_json::to_vec(&credential)?,
                )?;
            }
            let journal_path = app.join(crate::session::purge_owners::FILE_NAME);
            let old_journal = if version >= 38 {
                use crate::session::purge_owners::{PurgeCapture, PurgeOwner};
                crate::session::purge_owners::initialize(&app)?;
                let storage = crate::session::Storage::new_for_test_path(
                    "default",
                    app.join("sessions.json"),
                );
                let mut pending =
                    crate::session::Instance::new("pending purge", temp.path().to_str().unwrap());
                pending.source_profile = "default".into();
                pending.lifecycle_generation = 7;
                drop(PurgeOwner::record(
                    &storage,
                    &pending,
                    PurgeCapture::new(&pending)?,
                )?);
                Some(fs::read(&journal_path)?)
            } else {
                None
            };
            crate::migrations::run_migrations()?;
            assert_eq!(
                fs::read_to_string(app.join(".schema_version"))?,
                crate::migrations::current_schema_version().to_string()
            );
            let state = AppStateConfig::load()?;
            assert_eq!(state.sort_order, Some(SortOrder::Custom));
            assert_eq!(state.sessions_created, if seeded { 19 } else { 1 });
            assert!(state.sessions_created_seeded);
            assert_eq!(state.tips_seen, ["retained-tip"]);
            let rows: Value = serde_json::from_slice(&fs::read(app.join("sessions.json"))?)?;
            assert_eq!(rows[0]["lifecycle_generation"], 7);
            let identity = rows[0]["trash_lifecycle_id"].as_str().unwrap();
            uuid::Uuid::parse_str(identity)?;
            if let Some(previous) = row["trash_lifecycle_id"].as_str() {
                assert_eq!(identity, previous);
            }
            if version >= 38 {
                assert_eq!(rows[0]["failed_purge"]["trash_lifecycle_id"], identity);
                assert_eq!(rows[0]["failed_purge"]["forced"], true);
            }
            for name in ["serve.launch", "serve.rollback.launch"] {
                let converted: Value = serde_json::from_slice(&fs::read(app.join(name))?)?;
                assert_eq!(converted["pid"], launch["pid"]);
                assert_eq!(converted["instance_id"], launch["instance_id"]);
                assert_eq!(converted["auth_mode"], launch["auth_mode"]);
                assert_eq!(converted["has_passphrase"], true);
            }
            assert_eq!(
                serde_json::from_slice::<Value>(&fs::read(app.join("serve.rollback.passphrase"))?)?,
                credential
            );
            if let Some(bytes) = old_journal {
                assert_eq!(fs::read(&journal_path)?, bytes);
            }
            let names = [
                "state.toml",
                "sessions.json",
                "serve.launch",
                "serve.rollback.launch",
                "serve.passphrase",
                "serve.rollback.passphrase",
                crate::session::purge_owners::FILE_NAME,
            ];
            let bytes = names.map(|name| fs::read(app.join(name)).unwrap());
            run()?;
            run()?;
            crate::migrations::run_migrations()?;
            for (name, before) in names.into_iter().zip(bytes) {
                assert_eq!(
                    fs::read(app.join(name))?,
                    before,
                    "schema={version}, file={name}"
                );
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn schema_43_policy_replay_resumes_interrupted_records_without_recreating_ownership(
    ) -> anyhow::Result<()> {
        for (auth, secret, protected) in [
            ("none", None, false),
            ("token", Some("secondary login"), true),
            ("passphrase", None, true),
        ] {
            for step in 0..=3 {
                let temp = tempfile::tempdir()?;
                let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
                let app = crate::session::get_app_dir()?;
                fs::write(app.join(".schema_version"), "43")?;
                let old = json!({"schema":2,"pid":42,"instance_id":"retained-instance","auth_mode":auth,"remote":false});
                let mut current = old.clone();
                current["schema"] = json!(crate::cli::serve::SERVE_LAUNCH_SCHEMA);
                current["has_passphrase"] = json!(protected);
                fs::write(
                    app.join("serve.launch"),
                    serde_json::to_vec(if step >= 2 { &current } else { &old })?,
                )?;
                fs::write(
                    app.join("serve.rollback.launch"),
                    serde_json::to_vec(if step == 3 { &current } else { &old })?,
                )?;
                if let Some(secret) = secret {
                    fs::write(app.join("serve.passphrase"), secret)?;
                }
                let credential = format!("{{\n  \"pid\":42, \"instance_id\":\"retained-instance\", \"passphrase\":{}\n}}", serde_json::to_string(&secret)?);
                if step >= 1 {
                    fs::write(app.join("serve.rollback.passphrase"), &credential)?;
                }
                crate::migrations::run_migrations()?;
                for name in ["serve.launch", "serve.rollback.launch"] {
                    assert_eq!(
                        serde_json::from_slice::<Value>(&fs::read(app.join(name))?)?,
                        current
                    );
                }
                assert_eq!(
                    serde_json::from_slice::<Value>(&fs::read(
                        app.join("serve.rollback.passphrase")
                    )?)?["passphrase"],
                    json!(secret)
                );
                if step == 3 {
                    assert_eq!(
                        fs::read_to_string(app.join("serve.rollback.passphrase"))?,
                        credential
                    );
                }
                assert!(!app.join(crate::session::purge_owners::FILE_NAME).exists());
                let bytes = fs::read(app.join("serve.rollback.passphrase"))?;
                run()?;
                assert_eq!(fs::read(app.join("serve.rollback.passphrase"))?, bytes);
            }
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn inactive_malformed_policy_quarantines_original_bytes_without_clobbering_commits(
    ) -> anyhow::Result<()> {
        for version in [34, 43] {
            let temp = tempfile::tempdir()?;
            let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
            let app = crate::session::get_app_dir()?;
            fs::write(app.join(".schema_version"), version.to_string())?;
            let committed = json!({"schema":crate::cli::serve::SERVE_LAUNCH_SCHEMA,"pid":42,"instance_id":"committed","auth_mode":"token","has_passphrase":true}).to_string();
            fs::write(app.join("serve.launch"), &committed)?;
            fs::write(app.join("serve.rollback.launch"), "not JSON")?;
            fs::write(
                app.join("serve.rollback.passphrase"),
                "committed credential bytes",
            )?;
            if version == 34 {
                let step = crate::migrations::MIGRATIONS
                    .iter()
                    .find(|(version, ..)| *version == 35)
                    .unwrap();
                (step.2)()?;
                assert_eq!(
                    fs::read_to_string(app.join("serve.rollback.launch"))?,
                    "not JSON"
                );
            }
            crate::migrations::run_migrations()?;
            assert_eq!(
                crate::migrations::get_current_version(),
                crate::migrations::CURRENT_VERSION
            );
            assert_eq!(fs::read_to_string(app.join("serve.launch"))?, committed);
            assert_eq!(
                fs::read_to_string(app.join("serve.rollback.passphrase"))?,
                "committed credential bytes"
            );
            assert!(!app.join("serve.rollback.launch").try_exists()?);
            let quarantines = || -> anyhow::Result<Vec<std::path::PathBuf>> {
                Ok(fs::read_dir(&app)?
                    .collect::<std::io::Result<Vec<_>>>()?
                    .into_iter()
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with("serve.rollback.launch.quarantine-")
                    })
                    .map(|entry| entry.path())
                    .collect())
            };
            let preserved = quarantines()?;
            assert_eq!(preserved.len(), 1);
            assert_eq!(fs::read(&preserved[0])?, b"not JSON");
            run()?;
            crate::migrations::run_migrations()?;
            assert_eq!(quarantines()?, preserved);
            assert_eq!(fs::read(&preserved[0])?, b"not JSON");
            assert_eq!(fs::read_to_string(app.join("serve.launch"))?, committed);
            assert_eq!(
                fs::read(app.join("serve.rollback.passphrase"))?,
                b"committed credential bytes"
            );
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn unrelated_rollback_policy_retains_its_own_credentials() -> anyhow::Result<()> {
        let temp = tempfile::tempdir()?;
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir()?;
        fs::write(app.join(".schema_version"), "43")?;
        let current = json!({"schema":crate::cli::serve::SERVE_LAUNCH_SCHEMA,"pid":42,"instance_id":"current","auth_mode":"none","has_passphrase":false});
        let rollback = json!({"schema":2,"pid":99,"instance_id":"unrelated","auth_mode":"token","remote":false});
        fs::write(app.join("serve.launch"), current.to_string())?;
        fs::write(app.join("serve.rollback.launch"), rollback.to_string())?;
        let credential = "{\"pid\":99, \"instance_id\":\"unrelated\", \"passphrase\":\"retained rollback secret\"}\n";
        fs::write(app.join("serve.rollback.passphrase"), credential)?;
        crate::migrations::run_migrations()?;
        assert_eq!(
            fs::read_to_string(app.join("serve.launch"))?,
            current.to_string()
        );
        let migrated: Value =
            serde_json::from_slice(&fs::read(app.join("serve.rollback.launch"))?)?;
        assert_eq!(migrated["pid"], 99);
        assert_eq!(migrated["instance_id"], "unrelated");
        assert_eq!(migrated["auth_mode"], "token");
        assert_eq!(migrated["has_passphrase"], true);
        run()?;
        assert_eq!(
            fs::read_to_string(app.join("serve.rollback.passphrase"))?,
            credential
        );
        Ok(())
    }
    #[test]
    #[serial_test::serial]
    fn launch_only_reconciliation_refuses_unknown_and_malformed_schemas_without_writes(
    ) -> Result<()> {
        for invalid in [json!(99), json!(0), json!(-1), json!("4"), Value::Null] {
            let _home = crate::session::test_support::isolate_app_dir();
            let app = crate::session::get_app_dir()?;
            let current =
                json!({"schema":2,"pid":42,"instance_id":"current","auth_mode":"none"}).to_string();
            let retained =
                json!({"schema":invalid,"pid":42,"instance_id":"current","auth_mode":"none"})
                    .to_string();
            fs::write(app.join("serve.launch"), &current)?;
            fs::write(app.join("serve.rollback.launch"), &retained)?;
            let transaction =
                crate::daemon::lifecycle::Transaction::acquire_blocking_for(LOCK_WAIT)?;
            assert!(run_with_transaction(&transaction)
                .unwrap_err()
                .is::<UnsupportedLaunchSchema>());
            assert_eq!(fs::read_to_string(app.join("serve.launch"))?, current);
            assert_eq!(
                fs::read_to_string(app.join("serve.rollback.launch"))?,
                retained
            );
            assert!(!app.join("serve.rollback.passphrase").exists());
        }
        Ok(())
    }
}
