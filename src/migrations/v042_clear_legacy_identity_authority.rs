//! Clear inferred legacy runner identity and numeric process identity authority from schema-41 installations.

use anyhow::Result;

pub fn run() -> Result<()> {
    let changed = crate::process::worker_registry::migrate_kernel_identities()?;
    if changed > 0 {
        tracing::info!("v042: cleared {changed} legacy runtime identity claims");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};
    use std::fs;

    #[test]
    #[serial_test::serial]
    fn schema_41_clears_inferred_runner_identity_and_legacy_kernel_identity() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let workers = app.join("acp-workers");
        fs::create_dir_all(&workers).unwrap();
        fs::write(app.join(".schema_version"), "41").unwrap();

        let old_identity = json!({"boot_id": "old-boot", "start_id": 17});
        let new_identity = json!({"boot_id": "current-boot", "start_id": "birth-token"});
        let pid = std::process::id();
        let record = json!({
            "pid": pid,
            "runner_version": crate::process::worker_registry::RUNNER_VERSION - 1,
            "generation": 5,
            "process_identity": old_identity,
            "runner_identity": new_identity,
            "future_metadata": {"retained": [1, 2, 3]}
        });
        let record_path = workers.join("legacy.json");
        fs::write(&record_path, serde_json::to_vec(&record).unwrap()).unwrap();

        let current_record = json!({
            "pid": pid,
            "runner_version": crate::process::worker_registry::RUNNER_VERSION,
            "generation": 11,
            "process_identity": old_identity,
            "runner_identity": new_identity,
            "future_metadata": "current"
        });
        let current_record_path = workers.join("current.json");
        fs::write(
            &current_record_path,
            serde_json::to_vec(&current_record).unwrap(),
        )
        .unwrap();

        let no_birth = json!({"pid": pid, "process_identity": old_identity});
        let no_birth_path = workers.join("unattested.json");
        fs::write(&no_birth_path, serde_json::to_vec(&no_birth).unwrap()).unwrap();

        let fence = json!({
            "runner_pid": pid,
            "runner_generation": 5,
            "runner_process_identity": old_identity,
            "runner_identity": new_identity,
            "future_metadata": "retained"
        });
        let fence_path = workers.join("legacy.purge-fence");
        fs::write(&fence_path, serde_json::to_vec(&fence).unwrap()).unwrap();

        let current_fence = json!({
            "runner_pid": pid,
            "runner_generation": 11,
            "runner_identity": new_identity,
            "future_metadata": "current"
        });
        let current_fence_path = workers.join("current.purge-fence");
        fs::write(
            &current_fence_path,
            serde_json::to_vec(&current_fence).unwrap(),
        )
        .unwrap();

        let orphan_fence = json!({
            "runner_pid": pid,
            "runner_generation": 12,
            "runner_identity": new_identity,
            "runner_identity_attestation_version": 2
        });
        let orphan_fence_path = workers.join("orphan.purge-fence");
        fs::write(
            &orphan_fence_path,
            serde_json::to_vec(&orphan_fence).unwrap(),
        )
        .unwrap();

        let attestation_version =
            crate::process::worker_registry::PURGE_FENCE_IDENTITY_ATTESTATION_VERSION;
        let attested_fence = json!({
            "runner_pid": pid,
            "runner_generation": 13,
            "runner_identity": new_identity,
            "runner_identity_attestation_version": attestation_version
        });
        let attested_fence_path = workers.join("attested.purge-fence");
        fs::write(
            &attested_fence_path,
            serde_json::to_vec(&attested_fence).unwrap(),
        )
        .unwrap();

        let malformed = b"{\"process_identity\": {not-json";
        let malformed_path = workers.join("malformed.json");
        fs::write(&malformed_path, malformed).unwrap();

        super::super::run_migrations().unwrap();

        let migrated_record: Value =
            serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        assert!(migrated_record["process_identity"].is_null());
        assert!(migrated_record["runner_identity"].is_null());
        assert_eq!(migrated_record["pid"], pid);
        assert_eq!(
            migrated_record["future_metadata"],
            record["future_metadata"]
        );

        let migrated_unattested: Value =
            serde_json::from_slice(&fs::read(&no_birth_path).unwrap()).unwrap();
        assert!(migrated_unattested["process_identity"].is_null());
        assert!(migrated_unattested.get("runner_identity").is_none());

        let migrated_fence: Value =
            serde_json::from_slice(&fs::read(&fence_path).unwrap()).unwrap();
        assert!(migrated_fence["runner_process_identity"].is_null());
        assert!(migrated_fence["runner_identity"].is_null());
        assert_eq!(migrated_fence["runner_pid"], pid);
        assert_eq!(migrated_fence["future_metadata"], fence["future_metadata"]);
        let migrated_current: Value =
            serde_json::from_slice(&fs::read(&current_record_path).unwrap()).unwrap();
        assert!(migrated_current["process_identity"].is_null());
        assert_eq!(migrated_current["runner_identity"], new_identity);
        let migrated_current_fence: Value =
            serde_json::from_slice(&fs::read(&current_fence_path).unwrap()).unwrap();
        assert_eq!(migrated_current_fence["runner_identity"], new_identity);
        assert_eq!(
            migrated_current_fence["future_metadata"],
            current_fence["future_metadata"]
        );
        assert!(migrated_current_fence
            .get("runner_identity_attestation_version")
            .is_none());
        let migrated_orphan_fence: Value =
            serde_json::from_slice(&fs::read(&orphan_fence_path).unwrap()).unwrap();
        assert!(migrated_orphan_fence["runner_identity"].is_null());
        assert_eq!(
            migrated_orphan_fence["runner_identity_attestation_version"],
            2
        );
        let migrated_attested_fence: Value =
            serde_json::from_slice(&fs::read(&attested_fence_path).unwrap()).unwrap();
        assert_eq!(migrated_attested_fence["runner_identity"], new_identity);
        assert_eq!(
            migrated_attested_fence["runner_identity_attestation_version"],
            crate::process::worker_registry::PURGE_FENCE_IDENTITY_ATTESTATION_VERSION
        );
        assert_eq!(fs::read(&malformed_path).unwrap(), malformed);
        assert_eq!(crate::migrations::current_schema_version(), 42);
        assert_eq!(
            fs::read_to_string(app.join(".schema_version")).unwrap(),
            "42"
        );

        let after_first_run = [
            fs::read(&record_path).unwrap(),
            fs::read(&no_birth_path).unwrap(),
            fs::read(&fence_path).unwrap(),
            fs::read(&current_record_path).unwrap(),
            fs::read(&current_fence_path).unwrap(),
            fs::read(&orphan_fence_path).unwrap(),
            fs::read(&attested_fence_path).unwrap(),
            fs::read(&malformed_path).unwrap(),
        ];
        super::super::run_migrations().unwrap();
        assert_eq!(
            [
                fs::read(&record_path).unwrap(),
                fs::read(&no_birth_path).unwrap(),
                fs::read(&fence_path).unwrap(),
                fs::read(&current_record_path).unwrap(),
                fs::read(&current_fence_path).unwrap(),
                fs::read(&orphan_fence_path).unwrap(),
                fs::read(&attested_fence_path).unwrap(),
                fs::read(&malformed_path).unwrap(),
            ],
            after_first_run
        );
    }

    #[test]
    #[serial_test::serial]
    fn schema_40_runs_kernel_identity_migrations_and_preserves_current_runner_identity() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let workers = app.join("acp-workers");
        fs::create_dir_all(&workers).unwrap();
        fs::write(app.join(".schema_version"), "40").unwrap();

        let old_identity = json!({"boot_id": "old-boot", "start_id": 17});
        let inferred_identity = json!({"boot_id": "current-boot", "start_id": "birth-token"});
        let pid = std::process::id();
        let legacy = json!({
            "pid": pid,
            "runner_version": crate::process::worker_registry::RUNNER_VERSION - 1,
            "generation": 3,
            "process_identity": old_identity,
            "runner_identity": inferred_identity,
            "future_metadata": "preserved"
        });
        let legacy_path = workers.join("legacy.json");
        fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        let current_identity = json!({"boot_id": "current-boot", "start_id": "current-birth"});
        let current = json!({
            "pid": pid,
            "runner_version": crate::process::worker_registry::RUNNER_VERSION,
            "generation": 4,
            "process_identity": old_identity,
            "runner_identity": current_identity
        });
        let current_path = workers.join("current.json");
        fs::write(&current_path, serde_json::to_vec(&current).unwrap()).unwrap();

        super::super::run_migrations().unwrap();

        let migrated_legacy: Value =
            serde_json::from_slice(&fs::read(&legacy_path).unwrap()).unwrap();
        assert!(migrated_legacy["process_identity"].is_null());
        assert!(migrated_legacy["runner_identity"].is_null());
        assert_eq!(migrated_legacy["future_metadata"], "preserved");

        let migrated_current: Value =
            serde_json::from_slice(&fs::read(&current_path).unwrap()).unwrap();
        assert!(migrated_current["process_identity"].is_null());
        assert_eq!(migrated_current["runner_identity"], current_identity);
        assert_eq!(
            fs::read_to_string(app.join(".schema_version")).unwrap(),
            "42"
        );
    }

    #[test]
    #[serial_test::serial]
    fn schema_37_upgrade_keeps_prime_and_other_capture_migrations_on_the_existing_path() {
        let temp = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        let workers = app.join("acp-workers");
        fs::create_dir_all(&workers).unwrap();
        fs::write(app.join(".schema_version"), "37").unwrap();

        let profile = app.join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        let sessions_path = profile.join("sessions.json");
        fs::write(
            &sessions_path,
            serde_json::to_vec(&json!([
                {
                    "agent_session_id": "prime-session",
                    "active_execution": {
                        "capture": {
                            "Prime": {
                                "plan": {"store": "/store"},
                                "sidecar": {"SandboxDir": "/store"}
                            }
                        }
                    }
                },
                {
                    "agent_session_id": "hooks-session",
                    "active_execution": {"capture": {"Hooks": "/hooks"}}
                }
            ]))
            .unwrap(),
        )
        .unwrap();

        let worker = json!({
            "pid": std::process::id(),
            "runner_version": crate::process::worker_registry::RUNNER_VERSION - 1,
            "process_identity": {"boot_id": "old-boot", "start_id": 23},
            "future_metadata": "preserved"
        });
        let worker_path = workers.join("legacy.json");
        fs::write(&worker_path, serde_json::to_vec(&worker).unwrap()).unwrap();

        super::super::run_migrations().unwrap();

        let migrated_sessions: Value =
            serde_json::from_slice(&fs::read(&sessions_path).unwrap()).unwrap();
        assert_eq!(migrated_sessions[0]["agent_session_id"], "prime-session");
        assert!(migrated_sessions[0]
            .pointer("/active_execution/capture/Prime/sidecar")
            .is_none());
        assert!(migrated_sessions[0]
            .pointer("/active_execution/capture/Prime/runtime")
            .unwrap()
            .is_null());
        assert_eq!(
            migrated_sessions[0]["active_execution"]["capture"]["Prime"]["plan"]["store"],
            "/store"
        );
        assert_eq!(
            migrated_sessions[1]["active_execution"]["capture"]["Hooks"],
            "/hooks"
        );

        let migrated_worker: Value =
            serde_json::from_slice(&fs::read(&worker_path).unwrap()).unwrap();
        assert!(migrated_worker["process_identity"].is_null());
        assert!(migrated_worker["runner_identity"].is_null());
        assert_eq!(migrated_worker["future_metadata"], "preserved");
        assert_eq!(
            fs::read_to_string(app.join(".schema_version")).unwrap(),
            "42"
        );
    }
}
