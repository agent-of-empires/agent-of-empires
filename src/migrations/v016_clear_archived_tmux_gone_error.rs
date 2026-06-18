//! Migration v016: clear the spurious tmux-gone Error left on archived
//! sessions by builds shipped between #1868 (tear down tmux on archive) and
//! #2206 (stop the status poller flipping archived rows to Error).
//!
//! Those builds archived a session, killed its tmux, then let the status
//! poller observe the missing tmux and stamp `status = "error"` with a
//! "tmux session is gone" message. `last_error` is not persisted
//! (`#[serde(skip)]`), so on reload the row carries only `status = "error"`
//! and the message is gone; the in-process #2206 guard cannot recognize it.
//! This one-shot migration walks every sessions.json and demotes any archived
//! row still sitting at Error back to Idle. An archived row has no live tmux
//! by design, so an Error status on one can only be that spurious transition.
//!
//! ## Failure policy
//!
//! Per `AGENTS.md > Data Migrations`, a returned `Err` aborts boot. A
//! sessions.json that fails to parse is logged and skipped (a corrupt file
//! must not block boot or spam every launch). Only `get_app_dir` and
//! directory-read failures propagate.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                clear_archived_error(&entry.path().join("sessions.json"))?;
            }
        }
    }
    // Legacy top-level sessions.json (pre-profiles layout).
    clear_archived_error(&app_dir.join("sessions.json"))?;
    Ok(())
}

/// Demote any archived session still persisted at `status = "error"` back to
/// `"idle"`. Leaves non-archived rows and archived rows in any other status
/// untouched.
fn clear_archived_error(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            debug!("v016: failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let mut healed = 0usize;
    if let Some(array) = value.as_array_mut() {
        for instance in array.iter_mut() {
            if let Some(obj) = instance.as_object_mut() {
                let archived = obj.get("archived_at").is_some_and(|v| !v.is_null());
                let errored = obj.get("status").and_then(|v| v.as_str()) == Some("error");
                if archived && errored {
                    obj.insert(
                        "status".to_string(),
                        serde_json::Value::String("idle".to_string()),
                    );
                    healed += 1;
                }
            }
        }
    }

    if healed > 0 {
        crate::session::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())?;
        info!(
            "v016: cleared spurious archived Error on {healed} session(s) in {} (#2206)",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clears_only_archived_error_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[
                {"id":"a","status":"error","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"b","status":"error"},
                {"id":"c","status":"idle","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"d","status":"stopped","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"e","status":"error","archived_at":null}
            ]"#,
        )
        .unwrap();

        clear_archived_error(&path).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v.as_array().unwrap();
        // archived + error -> idle (the bug footprint)
        assert_eq!(arr[0]["status"], "idle");
        // non-archived error -> untouched
        assert_eq!(arr[1]["status"], "error");
        // archived non-error -> untouched
        assert_eq!(arr[2]["status"], "idle");
        assert_eq!(arr[3]["status"], "stopped");
        // explicit null archived_at counts as non-archived -> untouched
        assert_eq!(arr[4]["status"], "error");
    }

    #[test]
    fn missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        clear_archived_error(&dir.path().join("does-not-exist.json")).unwrap();
    }

    #[test]
    fn corrupt_file_is_skipped_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(&path, "{ not valid json").unwrap();
        // Must not error; a corrupt file is left untouched.
        clear_archived_error(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not valid json");
    }
}
