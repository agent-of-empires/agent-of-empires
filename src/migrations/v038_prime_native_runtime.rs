//! Retire Prime's JavaScript publication without inventing native ownership.

use anyhow::Result;
use serde_json::Value;
use std::{fs, path::Path};

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    for path in super::sessions_file::session_files(app_dir)? {
        if !path.try_exists()? {
            continue;
        }
        let directory = path.parent().expect("session files have a parent");
        let _lock = crate::session::acquire_storage_flock(
            directory,
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        let content = fs::read_to_string(&path)?;
        let mut rows: Value = match serde_json::from_str(&content) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "v038: skipping unreadable sessions");
                continue;
            }
        };
        let mut changed = false;
        for row in rows.as_array_mut().into_iter().flatten() {
            let Some(prime) = row
                .get_mut("active_execution")
                .and_then(|active| active.get_mut("capture"))
                .and_then(|capture| capture.get_mut("Prime"))
                .and_then(Value::as_object_mut)
            else {
                continue;
            };
            if prime.remove("sidecar").is_some() {
                prime.entry("runtime").or_insert(Value::Null);
                changed = true;
            }
        }
        if changed {
            crate::session::backup_before_migration(&path)?;
            crate::session::atomic_write(&path, serde_json::to_vec_pretty(&rows)?.as_slice())?;
            tracing::info!(path = %path.display(), "v038: retired Prime JavaScript publication");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upgrade_retains_conversation_and_other_publishers_without_fabricating_a_runtime() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("profiles/default");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("sessions.json");
        fs::write(&path, r#"[{"agent_session_id":"retained-root","active_execution":{"capture":{"Prime":{"plan":{"store":"/store","session_dir":"sessions","container_session_dir":"/root/.prime/agent/sessions","container_cwd":"/workspace"},"sidecar":{"SandboxDir":"/store"}}}}},{"active_execution":{"capture":{"Hooks":"/hooks"}}}]"#).unwrap();
        run_in(temp.path()).unwrap();
        run_in(temp.path()).unwrap();
        let rows: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(rows[0]["agent_session_id"], "retained-root");
        let mut active = rows[0]["active_execution"].clone();
        active["launch_id"] = serde_json::json!("legacy-launch");
        active["binding"] = serde_json::json!({"agent":"prime-agent","stores":["/store"],"configuration":[],"cwd":"/workspace","cwd_filesystem":"host","filesystem":"host"});
        active["container"] = Value::Null;
        let restored: crate::session::ActiveExecution = serde_json::from_value(active).unwrap();
        assert_eq!(restored.binding.agent, "prime-agent");
        assert_eq!(
            serde_json::to_value(restored.capture).unwrap()["Prime"]["runtime"],
            Value::Null
        );
        assert_eq!(rows[1]["active_execution"]["capture"]["Hooks"], "/hooks");
        let backups = crate::session::migration_backups(&path).unwrap();
        let old: Value = serde_json::from_slice(&fs::read(&backups[0].1).unwrap()).unwrap();
        assert_eq!(
            old[0]["active_execution"]["capture"]["Prime"]["sidecar"]["SandboxDir"],
            "/store"
        );
    }
}
