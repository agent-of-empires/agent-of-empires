//! Pin only control ledgers that attest the persisted native route.

use anyhow::Result;
use serde_json::Value;
use std::{fs, path::Path};

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn pin(runtime: &mut Value, workers: &Path) -> Result<bool> {
    let Some(object) = runtime.as_object() else {
        return Ok(false);
    };
    if object.contains_key("proof_path") {
        return Ok(false);
    }
    let mut route: crate::process::prime::PrimeRuntime =
        match serde_json::from_value(runtime.clone()) {
            Ok(route) => route,
            Err(error) => {
                tracing::warn!(%error, "v039: preserving an unreadable Prime route");
                return Ok(false);
            }
        };
    if let Err(error) = route.attest_control_path(workers) {
        tracing::warn!(directory = %route.directory.display(), %error, "v039: Prime control path remains unattested");
    }
    runtime["proof_path"] = serde_json::to_value(route.proof_path)?;
    Ok(true)
}

fn read_json(path: &Path) -> Result<Option<Value>> {
    let content = fs::read(path)?;
    match serde_json::from_slice(&content) {
        Ok(value) => Ok(Some(value)),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "v039: preserving unreadable persisted data");
            Ok(None)
        }
    }
}

fn persist(path: &Path, value: &Value) -> Result<()> {
    crate::session::backup_before_migration(path)?;
    crate::session::atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let workers = app_dir.canonicalize()?.join("acp-workers");
    for path in super::sessions_file::session_files(app_dir)? {
        if !path.try_exists()? {
            continue;
        }
        let _lock = crate::session::acquire_storage_flock(
            path.parent().unwrap(),
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        let Some(mut rows) = read_json(&path)? else {
            continue;
        };
        let mut changed = false;
        for row in rows.as_array_mut().into_iter().flatten() {
            let Some(capture) = row.pointer_mut("/active_execution/capture") else {
                continue;
            };
            if let Some(runtime) = capture.pointer_mut("/Prime/runtime") {
                changed |= pin(runtime, &workers)?;
            }
            if let Some(runtime) = capture.get_mut("PrimeStatus") {
                changed |= pin(runtime, &workers)?;
            }
        }
        if changed {
            persist(&path, &rows)?;
        }
    }
    if !workers.try_exists()? {
        return Ok(());
    }
    let paths = fs::read_dir(&workers)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    for path in paths {
        if path.extension().is_none_or(|extension| extension != "json") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|name| name.to_str()) else {
            continue;
        };
        if crate::process::worker_registry::validate_session_id(id).is_err() {
            continue;
        }
        crate::process::worker_registry::with_registry_lock(id, || {
            let Some(mut record) = read_json(&path)? else {
                return Ok(());
            };
            if record.get("agent_key").and_then(Value::as_str) != Some("prime-agent") {
                return Ok(());
            }
            if let Some(runtime) = record.get_mut("prime_runtime") {
                if pin(runtime, &workers)? {
                    persist(&path, &record)?;
                }
            }
            Ok(())
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    #[serial_test::serial]
    fn migration_preserves_unattested_ownership_and_pins_matching_routes_in_all_publications() {
        let root = tempfile::tempdir().unwrap();
        let _home = crate::session::test_support::isolate_app_dir_at(root.path());
        let app = crate::session::get_app_dir().unwrap();
        let route = crate::process::prime::PrimeRuntime::new(
            "/bin/false".into(),
            "/tmp/store".into(),
            None,
        )
        .unwrap();
        let proof = route.proof_path.as_ref().unwrap();
        fs::write(
            proof,
            serde_json::to_vec(
                &json!({"route":route.proof_route().unwrap(),"processes":[],"settled":false}),
            )
            .unwrap(),
        )
        .unwrap();
        let mut old = serde_json::to_value(&route).unwrap();
        old.as_object_mut().unwrap().remove("proof_path");
        let mut foreign = old.clone();
        foreign["program"] = json!("/bin/true");
        let profile = app.join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        let sessions = profile.join("sessions.json");
        fs::write(&sessions, serde_json::to_vec(&json!([
            {"agent_session_id":"retained-root","active_execution":{"capture":{"Prime":{"runtime":old}}}},
            {"active_execution":{"capture":{"PrimeStatus":foreign}}},
            {"active_execution":{"capture":{"Hooks":"unchanged"}}}
        ])).unwrap()).unwrap();
        let mut record = crate::process::worker_registry::WorkerRecord::new(
            "migrate-prime".into(),
            i32::MAX as u32,
            "/tmp/unused.sock".into(),
            "prime-agent".into(),
            "prime-agent".into(),
            "/tmp".into(),
            None,
            Vec::new(),
            Vec::new(),
            None,
            None,
        );
        record.agent_key = "prime-agent".into();
        record.prime_runtime = Some(serde_json::from_value(old).unwrap());
        record.generation = 7;
        crate::process::worker_registry::save(&record).unwrap();
        let record_path = app.join("acp-workers/migrate-prime.json");
        let mut legacy: Value = serde_json::from_slice(&fs::read(&record_path).unwrap()).unwrap();
        legacy["prime_runtime"]
            .as_object_mut()
            .unwrap()
            .remove("proof_path");
        fs::write(&record_path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        run_in(&app).unwrap();
        let first = fs::read(&sessions).unwrap();
        run_in(&app).unwrap();
        assert_eq!(fs::read(&sessions).unwrap(), first);
        let rows: Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(rows[0]["agent_session_id"], "retained-root");
        assert_eq!(
            rows[0]
                .pointer("/active_execution/capture/Prime/runtime/proof_path")
                .unwrap(),
            &json!(proof)
        );
        assert!(rows[1]
            .pointer("/active_execution/capture/PrimeStatus/proof_path")
            .unwrap()
            .is_null());
        assert_eq!(
            rows[1]
                .pointer("/active_execution/capture/PrimeStatus/program")
                .unwrap(),
            "/bin/true"
        );
        assert_eq!(rows[2]["active_execution"]["capture"]["Hooks"], "unchanged");
        let restored = crate::process::worker_registry::load("migrate-prime")
            .unwrap()
            .unwrap();
        assert_eq!((restored.pid, restored.generation), (record.pid, 7));
        assert_eq!(
            restored.prime_runtime.unwrap().proof_path.as_ref(),
            Some(proof)
        );
        assert_eq!(
            crate::session::migration_backups(&sessions).unwrap().len(),
            1
        );
    }
}
