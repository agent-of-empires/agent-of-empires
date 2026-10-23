use anyhow::{Context, Result};
use std::path::Path;

pub fn run() -> Result<()> {
    super::recovery::reconcile_repaired_components()
}

pub(super) fn reconcile_sidebar_settings() -> Result<()> {
    let app = crate::session::get_app_dir()?;
    migrate_config_file(&app.join("config.toml"))?;
    let profiles = app.join("profiles");
    if profiles.exists() {
        for entry in std::fs::read_dir(profiles)? {
            let entry = entry?;
            if entry.path().is_dir() {
                migrate_config_file(&entry.path().join("config.toml"))?;
            }
        }
    }
    Ok(())
}

/// Leave unreadable live configuration untouched; propagate failures after a planned edit.
fn migrate_config_file(path: &Path) -> Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            tracing::warn!(target: "migrations.v046", path = %path.display(), %error, "Could not read config for canonical sidebar reconciliation");
            return Ok(());
        }
    };
    let mut config: toml_edit::DocumentMut = match content.parse() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(target: "migrations.v046", path = %path.display(), %error, "Preserved unreadable live config");
            return Ok(());
        }
    };
    let removed = config
        .get_mut("session")
        .and_then(toml_edit::Item::as_table_mut)
        .and_then(|session| session.remove("daemon_sidebar"));
    if removed.is_some() {
        crate::session::backup_before_migration(path)?;
        crate::session::atomic_write(path, config.to_string().as_bytes())
            .with_context(|| format!("Writing {}", path.display()))?;
        tracing::info!(target: "migrations.v046", path = %path.display(), "Removed session.daemon_sidebar");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[serial_test::serial]
    async fn quarantined_components_keep_bytes_and_named_retyping_retries_repaired_fields(
    ) -> Result<()> {
        use crate::process::worker_registry::{self, ScopeOwnership, WorkerRecord};
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        std::fs::write(app.join(".schema_version"), "40")?;
        let healthy = crate::session::Storage::new_unwatched("healthy")?;
        let damaged = crate::session::Storage::new_unwatched("damaged")?;
        let row = crate::session::Instance::new("healthy", "/tmp/repo");
        healthy.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        std::fs::write(damaged.sessions_path(), b"{")?;
        let workers = worker_registry::workers_dir()?;
        let worker_path = workers.join("deferred.json");
        std::fs::write(&worker_path, b"{")?;
        crate::migrations::run_migrations()?;
        assert_eq!(std::fs::read(damaged.sessions_path())?, b"{");
        assert_eq!(std::fs::read(&worker_path)?, b"{");
        let state = crate::server::test_support::build_test_app_state(Vec::new());
        crate::server::test_support::accept_runtime_read_cache_for_test(&state).await;
        assert!(matches!(
            *state.canonical_health.read().await,
            crate::daemon::RuntimeHealth::Degraded { .. }
        ));
        assert!(state
            .instances
            .read()
            .await
            .iter()
            .any(|instance| instance.id == row.id));
        assert!(state
            .runtime_read_cache
            .read()
            .unwrap()
            .health
            .unreadable_profiles
            .contains("damaged"));
        assert!(
            !state
                .runtime
                .publish(&state)
                .await?
                .value
                .contents
                .capabilities
                .mutations
        );
        let mut repaired =
            serde_json::to_value(crate::session::Instance::new("repaired", "/tmp/repo"))?;
        repaired.as_object_mut().unwrap().remove("terminal_scopes");
        std::fs::write(
            damaged.sessions_path(),
            serde_json::to_vec(&serde_json::json!([repaired]))?,
        )?;
        let worker = WorkerRecord::new(
            "deferred".into(),
            i32::MAX as u32,
            workers.join("deferred.sock"),
            "agent".into(),
            "agent".into(),
            "/tmp".into(),
            None,
            vec![],
            vec![],
            None,
            Some("damaged".into()),
        )
        .with_generation(7);
        let mut worker = serde_json::to_value(worker)?;
        worker.as_object_mut().unwrap().remove("scope");
        worker.as_object_mut().unwrap().remove("runner_identity");
        let repaired_worker = serde_json::to_vec(&worker)?;
        std::fs::write(&worker_path, &repaired_worker)?;
        assert!(worker_registry::load_strict("deferred").is_err());
        for authority in [
            worker_registry::ResumeAuthority::Automatic,
            worker_registry::ResumeAuthority::Explicit,
        ] {
            assert!(worker_registry::admit_resume("deferred", authority, &None).is_err());
        }
        crate::migrations::recovery::reconcile_repaired_components()?;
        let worker = worker_registry::load_strict("deferred")?.unwrap();
        assert_eq!(worker.scope, ScopeOwnership::Unresolved);
        assert!(worker.runner_identity.is_none());
        assert!(crate::process::systemd::scope_stopped("deferred", &worker.scope).is_err());
        let rows: serde_json::Value =
            serde_json::from_slice(&std::fs::read(damaged.sessions_path())?)?;
        assert!(rows[0]["terminal_scopes"].is_null());
        assert!(crate::session::migration_backups(&worker_path)?
            .iter()
            .any(|(_, path)| std::fs::read(path).is_ok_and(|bytes| bytes == repaired_worker)));
        let bytes = std::fs::read(&worker_path)?;
        crate::migrations::recovery::reconcile_repaired_components()?;
        assert_eq!(std::fs::read(worker_path)?, bytes);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn parseable_invalid_scope_proof_remains_fatal_without_advancing_the_marker() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        std::fs::write(app.join(".schema_version"), "44")?;
        let mut row =
            serde_json::to_value(crate::session::Instance::new("invalid proof", "/tmp/repo"))?;
        row["terminal_scopes"] = serde_json::json!([{"state":"guessed"}]);
        let path = app.join("sessions.json");
        let bytes = serde_json::to_vec(&serde_json::json!([row]))?;
        std::fs::write(&path, &bytes)?;
        assert!(crate::migrations::run_migrations().is_err());
        assert_eq!(crate::migrations::get_current_version(), 44);
        assert_eq!(std::fs::read(path)?, bytes);
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    #[serial_test::serial]
    fn published_and_private_markers_receive_public_transforms_without_inventing_authority(
    ) -> Result<()> {
        use crate::process::worker_registry::{self, WorkerRecord};
        use serde_json::{json, Value};
        for (version, private) in [
            (37, false),
            (38, false),
            (39, false),
            (40, false),
            (38, true),
            (39, true),
            (40, true),
            (45, true),
        ] {
            let home = tempfile::TempDir::with_prefix_in("aoe-lineage-", "/tmp")?;
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir()?;
            std::fs::write(app.join(".schema_version"), version.to_string())?;
            let config = app.join("config.toml");
            std::fs::write(
                &config,
                "# retained\n[session]\ndaemon_sidebar = false\ndefault_tool = 'codex'\n",
            )?;
            let mut row =
                serde_json::to_value(crate::session::Instance::new("retained", "/tmp/repo"))?;
            row["agent_session_id"] = json!("retained-root");
            row["extension"] = json!({"keep":42});
            row["active_execution"] = json!({
                "launch_id":"legacy-launch",
                "binding":{"agent":"prime-agent","stores":["/store"],"configuration":[],"cwd":"/workspace","cwd_filesystem":"host","filesystem":"host"},
                "container":null,
                "capture":{"Prime":{"plan":{"store":"/store","session_dir":"sessions","container_session_dir":"/root/.prime/agent/sessions","container_cwd":"/workspace"},"sidecar":{"SandboxDir":"/store"}}}
            });
            let sessions = app.join("sessions.json");
            std::fs::write(&sessions, serde_json::to_vec(&json!([row]))?)?;
            let workers = worker_registry::workers_dir()?;
            let socket = workers.join("attested.sock");
            let control = crate::process::worker::control_socket_sibling(&socket);
            let _listener = std::os::unix::net::UnixListener::bind(control)?;
            let record = WorkerRecord::new(
                "attested".into(),
                std::process::id(),
                socket,
                "agent".into(),
                "agent".into(),
                "/tmp".into(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .with_generation(7);
            let mut historical = serde_json::to_value(record)?;
            historical
                .as_object_mut()
                .unwrap()
                .remove("runner_identity");
            if !private {
                historical.as_object_mut().unwrap().remove("scope");
            }
            let record_path = workers.join("attested.json");
            std::fs::write(&record_path, serde_json::to_vec(&historical)?)?;
            let route = crate::process::prime::PrimeRuntime::new(
                "/bin/false".into(),
                "/tmp/store".into(),
                None,
            )?;
            let proof = route.proof_path.as_ref().unwrap().clone();
            std::fs::write(
                &proof,
                serde_json::to_vec(
                    &json!({"route":route.proof_route()?,"processes":[],"settled":false,"extension":"retained"}),
                )?,
            )?;
            let mut legacy_route = serde_json::to_value(&route)?;
            legacy_route.as_object_mut().unwrap().remove("proof_path");
            let mut native = WorkerRecord::new(
                "native".into(),
                i32::MAX as u32,
                workers.join("native.sock"),
                "prime-agent".into(),
                "prime-agent".into(),
                "/tmp".into(),
                None,
                vec![],
                vec![],
                None,
                None,
            )
            .with_generation(8);
            native.prime_runtime = Some(serde_json::from_value(legacy_route)?);
            let native_path = workers.join("native.json");
            let mut native = serde_json::to_value(native)?;
            native["prime_runtime"]
                .as_object_mut()
                .unwrap()
                .remove("proof_path");
            native.as_object_mut().unwrap().remove("runner_identity");
            std::fs::write(&native_path, serde_json::to_vec(&native)?)?;
            crate::migrations::run_migrations()?;
            assert_eq!(
                crate::migrations::get_current_version(),
                crate::migrations::CURRENT_VERSION
            );
            let migrated: Value = serde_json::from_slice(&std::fs::read(&sessions)?)?;
            assert_eq!(migrated[0]["agent_session_id"], "retained-root");
            assert_eq!(migrated[0]["extension"], json!({"keep":42}));
            assert!(migrated[0]
                .pointer("/active_execution/capture/Prime/sidecar")
                .is_none());
            assert!(migrated[0]
                .pointer("/active_execution/capture/Prime/runtime")
                .unwrap()
                .is_null());
            let attested: Value = serde_json::from_slice(&std::fs::read(&record_path)?)?;
            assert_eq!(
                attested["runner_identity"],
                serde_json::to_value(crate::process::process_birth_identity(std::process::id())?)?
            );
            assert_eq!(
                attested["scope"]["state"],
                if private { "unscoped" } else { "unresolved" }
            );
            let native: Value = serde_json::from_slice(&std::fs::read(&native_path)?)?;
            assert_eq!(native["prime_runtime"]["proof_path"], json!(proof));
            assert!(native["runner_identity"].is_null());
            let proof_value: Value = serde_json::from_slice(&std::fs::read(&proof)?)?;
            assert!(proof_value["epoch"].is_null());
            assert!(proof_value["launch_epoch"].is_null());
            assert_eq!(proof_value["extension"], "retained");
            let config_value: toml::Value = toml::from_str(&std::fs::read_to_string(&config)?)?;
            assert!(config_value["session"].get("daemon_sidebar").is_none());
            assert_eq!(
                config_value["session"]["default_tool"].as_str(),
                Some("codex")
            );
            assert!(!crate::session::migration_backups(&sessions)?.is_empty());
            assert!(!crate::session::migration_backups(&record_path)?.is_empty());
            assert!(!crate::session::migration_backups(&config)?.is_empty());
            let journal = app.join(crate::session::purge_owners::FILE_NAME);
            assert_eq!(journal.exists(), version < 41);
            let journal_before = if journal.exists() {
                Some(std::fs::read(&journal)?)
            } else {
                None
            };
            let paths = [&sessions, &record_path, &native_path, &proof, &config];
            let bytes = paths
                .iter()
                .map(std::fs::read)
                .collect::<std::io::Result<Vec<_>>>()?;
            crate::migrations::recovery::reconcile_repaired_components()?;
            let _inventory_denied =
                crate::migrations::legacy_scope_recovery::test_support::deny_inventory();
            if journal_before.is_some() && private {
                crate::migrations::run_migrations_announced(None)?;
            } else {
                assert!(crate::migrations::run_migrations_announced(None).is_err());
            }
            for (path, expected) in paths.into_iter().zip(bytes) {
                assert_eq!(
                    std::fs::read(path)?,
                    expected,
                    "version {version}, private {private}"
                );
            }
            assert_eq!(journal.exists(), version < 41);
            if let Some(before) = journal_before {
                assert_eq!(std::fs::read(journal)?, before);
            }
        }
        Ok(())
    }

    #[test]
    fn removing_sidebar_opt_out_preserves_other_settings_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "# global comment\n[session] # session comment\ndaemon_sidebar = false\ndefault_tool = 'codex' # keep tool comment\n[status_hooks]\nenabled = true # keep hook comment\n",
        )
        .unwrap();
        migrate_config_file(&path).unwrap();
        let migrated = std::fs::read_to_string(&path).unwrap();
        let config: toml::Value = toml::from_str(&migrated).unwrap();
        assert!(migrated.contains("# global comment"));
        assert!(migrated.contains("[session] # session comment"));
        assert!(migrated.contains("default_tool = 'codex' # keep tool comment"));
        assert!(migrated.contains("enabled = true # keep hook comment"));
        assert!(config["session"].get("daemon_sidebar").is_none());
        assert_eq!(config["session"]["default_tool"].as_str(), Some("codex"));
        assert_eq!(config["status_hooks"]["enabled"].as_bool(), Some(true));
        migrate_config_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated);
    }

    #[test]
    fn an_unparseable_config_is_reported_and_left_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let broken = "[session\ndaemon_sidebar = false\n";
        std::fs::write(&path, broken).unwrap();
        migrate_config_file(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "a config the user must repair is never rewritten by the migration"
        );
    }
}
