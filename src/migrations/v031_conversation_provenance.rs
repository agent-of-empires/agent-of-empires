//! Existing IDs retain unknown provenance; status labels cannot establish a store.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
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

fn unknown_binding(object: &mut Map<String, Value>, key: &str, sid: Option<String>) -> bool {
    let Some(sid) = sid.filter(|sid| !sid.trim().is_empty()) else {
        return false;
    };
    if object.get(key).is_some_and(|binding| !binding.is_null()) {
        return false;
    }
    object.insert(
        key.into(),
        serde_json::json!({
            "session_id": sid, "execution": null, "provenance": "unknown"
        }),
    );
    true
}

fn migrate_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let directory = path.parent().context("sessions path has no parent")?;
    let _lock =
        crate::session::acquire_storage_flock(directory, crate::session::STORAGE_LOCK_FILENAME)?;
    let content = fs::read_to_string(path)?;
    let mut value: Value = match serde_json::from_str(&content) {
        Ok(value) => value,
        Err(error) => {
            tracing::debug!("v031: cannot parse {}: {error}; skipping", path.display());
            return Ok(());
        }
    };
    let mut changed = false;
    if let Some(instances) = value.as_array_mut() {
        for instance in instances {
            let Some(object) = instance.as_object_mut() else {
                continue;
            };
            let sid = object
                .get("agent_session_id")
                .and_then(Value::as_str)
                .map(str::to_owned);
            changed |= unknown_binding(object, "agent_session_binding", sid);
            let intent = object.get("resume_intent");
            let target = match intent
                .and_then(|intent| intent.get("kind"))
                .and_then(Value::as_str)
            {
                Some("Use") => intent
                    .and_then(|intent| intent.get("value"))
                    .and_then(Value::as_str),
                Some("Fork") => intent
                    .and_then(|intent| intent.get("value"))
                    .and_then(|value| value.get("from"))
                    .and_then(Value::as_str),
                _ => None,
            }
            .map(str::to_owned);
            changed |= unknown_binding(object, "resume_binding", target);
            if let Some(prior) = object
                .get_mut("prior_tool_session_ids")
                .and_then(Value::as_object_mut)
            {
                for value in prior.values_mut() {
                    if let Some(prior) = value.as_object_mut() {
                        let sid = prior
                            .get("agent_session_id")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        changed |= unknown_binding(prior, "agent_session_binding", sid);
                    }
                }
            }
        }
    }
    if changed {
        crate::session::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_ids_and_known_bindings_without_inferring_legacy_identity() {
        let root = tempfile::tempdir().unwrap();
        let profile = root.path().join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        let original = serde_json::json!([
            {"agent_session_id":"old", "tool":"claude", "detect_as":"codex", "resume_intent":{"kind":"Use","value":"target"}, "prior_tool_session_ids":{"claude":{"agent_session_id":"parked","acp_session_id":"acp"}}},
            {"agent_session_id":"child", "resume_intent":{"kind":"Fork","value":{"from":"parent"}}},
            {"agent_session_id":"known", "agent_session_binding":{"session_id":"known","execution":{"agent":"claude","stores":["/store"],"cwd":"/work","filesystem":"host"},"provenance":"observed"}}
        ]);
        for path in [
            root.path().join("sessions.json"),
            profile.join("sessions.json"),
        ] {
            fs::write(path, serde_json::to_vec(&original).unwrap()).unwrap();
        }
        run_in(root.path()).unwrap();
        for path in [
            root.path().join("sessions.json"),
            profile.join("sessions.json"),
        ] {
            let bytes = fs::read(&path).unwrap();
            let value: Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(value[0]["agent_session_id"], "old");
            assert_eq!(value[0]["agent_session_binding"]["execution"], Value::Null);
            assert_eq!(value[0]["resume_binding"]["session_id"], "target");
            assert_eq!(
                value[0]["prior_tool_session_ids"]["claude"]["agent_session_binding"]["session_id"],
                "parked"
            );
            assert_eq!(
                value[0]["prior_tool_session_ids"]["claude"]["acp_session_id"],
                "acp"
            );
            assert_eq!(value[1]["resume_intent"], original[1]["resume_intent"]);
            assert_eq!(value[1]["resume_binding"]["session_id"], "parent");
            assert_eq!(value[2], original[2]);
            migrate_file(&path).unwrap();
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }
}
