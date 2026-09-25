//! Migration v023: demote structured (ACP) sessions still persisted at
//! `status = "error"` back to Idle.
//!
//! Builds whose poller ran the sandbox-dead check before bailing on
//! `is_structured()` stamped a not-running container as a session error. A
//! structured session's container belongs to its ACP worker rather than to a
//! tmux pane, so that reading said nothing about the session, and the daemon
//! never persisted structured status in those builds. A sessions.json that
//! fails to read or parse is logged and skipped.

use super::sessions_file;
use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    for path in sessions_file::session_files(app_dir)? {
        clear_structured_error(&path)?;
    }
    Ok(())
}

/// Demote any structured row persisted at `status = "error"` back to Idle. A
/// terminal row keeps its Error: the tmux poller is a real producer for those.
/// `view` is skipped in serialization when it holds the default `Terminal`, so
/// an absent field means terminal, not structured.
fn clear_structured_error(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    // A read failure is skipped for the same reason a parse failure is: this
    // heal is best-effort, and a permissions hiccup or a non-UTF-8 file must
    // not abort boot.
    let content = match fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) => {
            debug!("v023: failed to read {}: {e}, skipping", path.display());
            return Ok(());
        }
    };
    let healed = sessions_file::heal_rows(path, &content, |row| {
        let structured = row.get("view").and_then(|v| v.as_str()) == Some("structured");
        let spurious = structured && sessions_file::status(row) == Some("error");
        if spurious {
            sessions_file::settle_to_idle(row);
        }
        spurious
    })?;
    if healed > 0 {
        info!(
            "v023: cleared spurious container Error on {healed} structured session(s) in {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clears_only_structured_error_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[
                {"id":"a","status":"error","view":"structured"},
                {"id":"b","status":"error","view":"terminal"},
                {"id":"c","status":"error"},
                {"id":"d","status":"idle","view":"structured"},
                {"id":"e","status":"stopped","view":"structured"}
            ]"#,
        )
        .unwrap();

        clear_structured_error(&path).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v.as_array().unwrap();
        // structured + error -> idle (the bug footprint)
        assert_eq!(arr[0]["status"], "idle");
        // explicit terminal error -> untouched (real tmux producer)
        assert_eq!(arr[1]["status"], "error");
        // absent view means terminal (View::is_terminal skips it) -> untouched
        assert_eq!(arr[2]["status"], "error");
        // structured non-error -> untouched
        assert_eq!(arr[3]["status"], "idle");
        assert_eq!(arr[4]["status"], "stopped");
    }

    #[test]
    fn is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[{"id":"a","status":"error","view":"structured"}]"#,
        )
        .unwrap();
        clear_structured_error(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        clear_structured_error(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);
    }

    #[test]
    fn missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        clear_structured_error(&dir.path().join("does-not-exist.json")).unwrap();
    }

    #[test]
    fn corrupt_file_is_skipped_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(&path, "{ not valid json").unwrap();
        clear_structured_error(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "{ not valid json");
    }

    #[test]
    fn walks_profiles_and_legacy_layouts() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("work");
        fs::create_dir_all(&profile).unwrap();
        let row = r#"[{"id":"a","status":"error","view":"structured"}]"#;
        fs::write(profile.join("sessions.json"), row).unwrap();
        fs::write(dir.path().join("sessions.json"), row).unwrap();

        run_in(dir.path()).unwrap();

        for p in [
            profile.join("sessions.json"),
            dir.path().join("sessions.json"),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
            assert_eq!(v[0]["status"], "idle", "{}", p.display());
        }
    }
}
