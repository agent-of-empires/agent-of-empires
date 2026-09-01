//! Clear session IDs produced by attribution paths that cannot prove ownership.

use anyhow::{Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                clear_file(&entry.path().join("sessions.json"))?;
            }
        }
    }
    clear_file(&app_dir.join("sessions.json"))
}

fn clear_file(path: &Path) -> Result<()> {
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("v027: reading {}", path.display()));
        }
    };
    let (_, _, needs_change) = migrate_content(path, &content)?;
    if !needs_change {
        return Ok(());
    }

    let parent = path
        .parent()
        .with_context(|| format!("v027: {} has no parent directory", path.display()))?;
    let _lock = crate::session::acquire_storage_flock(parent, ".storage.lock")?;
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("v027: reading {}", path.display()));
        }
    };
    let (value, cleared, changed) = migrate_content(path, &content)?;
    if changed {
        crate::session::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())?;
        info!(
            "v027: cleared {cleared} unverified session id(s) in {}",
            path.display()
        );
    }
    Ok(())
}

fn migrate_content(path: &Path, content: &str) -> Result<(serde_json::Value, usize, bool)> {
    let mut value: serde_json::Value = serde_json::from_str(content)
        .with_context(|| format!("v027: parsing {}", path.display()))?;
    let instances = value
        .as_array_mut()
        .with_context(|| format!("v027: {} root is not an array", path.display()))?;

    let mut cleared = 0usize;
    let mut changed = false;
    for instance in instances {
        let Some(object) = instance.as_object_mut() else {
            continue;
        };
        let sandboxed = object
            .get("sandbox_info")
            .and_then(|value| value.get("enabled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        let detected = object
            .get("detect_as")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .or_else(|| object.get("tool").and_then(serde_json::Value::as_str));
        let explicit = object
            .get("resume_intent")
            .and_then(|value| value.get("kind"))
            .and_then(serde_json::Value::as_str)
            .is_some_and(|kind| matches!(kind, "Use" | "Fork"));
        let proven = explicit
            || matches!(detected, Some("pi" | "omp"))
            || (detected == Some("codex") && sandboxed);

        if !proven && object.remove("agent_session_id").is_some() {
            object.remove("resume_probe_failed_sid");
            object.remove("pi_session_path");
            cleared += 1;
            changed = true;
        }

        let remove_prior = if let Some(prior) = object
            .get_mut("prior_tool_session_ids")
            .and_then(serde_json::Value::as_object_mut)
        {
            let before = prior.len();
            prior.retain(|tool, _| {
                matches!(tool.as_str(), "pi" | "omp") || (tool == "codex" && sandboxed)
            });
            changed |= prior.len() != before;
            prior.is_empty()
        } else {
            false
        };
        if remove_prior {
            object.remove("prior_tool_session_ids");
            changed = true;
        }
    }

    Ok((value, cleared, changed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clears_scan_ids_and_preserves_proven_ids() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[
              {"id":"a","tool":"claude","agent_session_id":"scan"},
              {"id":"b","tool":"custom","detect_as":"gemini","agent_session_id":"scan"},
              {"id":"c","tool":"pi","agent_session_id":"pin"},
              {"id":"d","tool":"codex","sandbox_info":{"enabled":true},"agent_session_id":"private"},
              {"id":"e","tool":"cursor","agent_session_id":"explicit","resume_intent":{"kind":"Use","value":"explicit"}},
              {"id":"f","tool":"claude","prior_tool_session_ids":{"vibe":{"agent_session_id":"scan"},"omp":{"agent_session_id":"pane"}}}
            ]"#,
        )
        .unwrap();

        clear_file(&path).unwrap();

        let value: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap();
        let rows = value.as_array().unwrap();
        assert!(rows[0].get("agent_session_id").is_none());
        assert!(rows[1].get("agent_session_id").is_none());
        assert_eq!(rows[2]["agent_session_id"], "pin");
        assert_eq!(rows[3]["agent_session_id"], "private");
        assert_eq!(rows[4]["agent_session_id"], "explicit");
        assert!(rows[5]["prior_tool_session_ids"].get("vibe").is_none());
        assert_eq!(
            rows[5]["prior_tool_session_ids"]["omp"]["agent_session_id"],
            "pane"
        );
    }

    #[test]
    fn is_idempotent_and_rejects_corrupt_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[{"id":"a","tool":"vibe","agent_session_id":"scan"}]"#,
        )
        .unwrap();
        clear_file(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        clear_file(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);

        let corrupt = dir.path().join("corrupt.json");
        fs::write(&corrupt, "not json").unwrap();
        assert!(clear_file(&corrupt).is_err());
        assert_eq!(fs::read_to_string(&corrupt).unwrap(), "not json");

        let non_array = dir.path().join("non-array.json");
        fs::write(&non_array, "{}").unwrap();
        assert!(clear_file(&non_array).is_err());
    }
}
