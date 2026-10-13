//! Reconcile launch policy for installations already at a prior PR schema.

pub fn run() -> anyhow::Result<()> {
    super::v039_serve_passphrase_policy::run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::config::{update_app_state, AppStateConfig, SortOrder};
    use serde_json::{json, Value};
    use std::fs;

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
            assert_eq!(fs::read_to_string(app.join(".schema_version"))?, "45");
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
    fn malformed_policy_never_advertises_the_published_marker_or_clobbers_a_commit(
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
                    .find(|step| step.version == 35)
                    .unwrap();
                (step.run)()?;
                assert_eq!(
                    fs::read_to_string(app.join("serve.rollback.launch"))?,
                    "not JSON"
                );
            }
            assert!(crate::migrations::run_migrations().is_err());
            assert_eq!(
                fs::read_to_string(app.join(".schema_version"))?,
                if version == 34 { "38" } else { "43" }
            );
            assert_eq!(fs::read_to_string(app.join("serve.launch"))?, committed);
            assert_eq!(
                fs::read_to_string(app.join("serve.rollback.passphrase"))?,
                "committed credential bytes"
            );
            assert_eq!(
                fs::read_to_string(app.join("serve.rollback.launch"))?,
                "not JSON"
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
}
