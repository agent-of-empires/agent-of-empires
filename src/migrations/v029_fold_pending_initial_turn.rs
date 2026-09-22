//! Migration v029: fold `pending_initial_turn` + `pending_initial_turn_attachments`
//! into one `pending_initial_turn` record.
//!
//! The two flat fields tracked one queued turn (a session's create-time
//! initial prompt, or a rate-limit resume continuation) between them.
//! `Instance::pending_initial_turn` needed a third piece of turn state
//! (`synthesized`, so the transcript model can skip rendering a resend the
//! user already saw once) and the existing code comment called for folding
//! into a typed record at that point rather than adding a fourth flat field.
//!
//! Every pre-existing row predates the `synthesized` flag, so it folds to
//! `false` (the field only ever meant a create-time initial turn until now).

use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;
use tracing::{debug, info};

/// Migration entry point: fold `pending_initial_turn` under the app dir.
pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

/// Walk every profile's `sessions.json` plus the legacy top-level one under
/// `app_dir`. Split from `run` so tests can point it at a temp dir.
pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                fold_pending_initial_turn(&entry.path().join("sessions.json"))?;
            }
        }
    }
    // Legacy top-level sessions.json (pre-profiles layout).
    fold_pending_initial_turn(&app_dir.join("sessions.json"))?;
    Ok(())
}

/// Fold `pending_initial_turn: Option<String>` plus a sibling
/// `pending_initial_turn_attachments: Vec<..>` into a single
/// `pending_initial_turn: { text, attachments, synthesized: false }` object.
/// Leaves rows with no `pending_initial_turn` string untouched.
fn fold_pending_initial_turn(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("sessions path has no parent: {}", path.display()))?;
    // The lock `Storage::update` holds, kept across read and write so a
    // concurrent update is neither lost nor able to undo the fold.
    let _flock = crate::session::acquire_storage_flock(dir, crate::session::STORAGE_LOCK_FILENAME)?;
    let content = fs::read_to_string(path)?;
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            debug!("v029: failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let mut folded = 0usize;
    if let Some(array) = value.as_array_mut() {
        for instance in array.iter_mut() {
            let Some(obj) = instance.as_object_mut() else {
                continue;
            };
            let Some(text) = obj
                .get("pending_initial_turn")
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                continue;
            };
            let attachments = obj
                .remove("pending_initial_turn_attachments")
                .unwrap_or(serde_json::Value::Array(Vec::new()));
            obj.insert(
                "pending_initial_turn".to_string(),
                serde_json::json!({
                    "text": text,
                    "attachments": attachments,
                    "synthesized": false,
                }),
            );
            folded += 1;
        }
    }

    if folded > 0 {
        crate::session::atomic_write(path, serde_json::to_string_pretty(&value)?.as_bytes())?;
        info!(
            "v029: folded pending_initial_turn on {folded} session(s) in {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folds_text_and_attachments_into_one_record() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[
                {"id":"a","pending_initial_turn":"run the nightly task","pending_initial_turn_attachments":[{"id":"att-1","kind":"image","mime_type":"image/png","name":"x.png","size":10}]},
                {"id":"b","pending_initial_turn":"no attachments"},
                {"id":"c"}
            ]"#,
        )
        .unwrap();

        fold_pending_initial_turn(&path).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v.as_array().unwrap();
        assert_eq!(
            arr[0]["pending_initial_turn"]["text"],
            "run the nightly task"
        );
        assert_eq!(arr[0]["pending_initial_turn"]["synthesized"], false);
        assert_eq!(
            arr[0]["pending_initial_turn"]["attachments"][0]["id"],
            "att-1"
        );
        assert!(arr[0].get("pending_initial_turn_attachments").is_none());
        assert_eq!(arr[1]["pending_initial_turn"]["text"], "no attachments");
        assert_eq!(
            arr[1]["pending_initial_turn"]["attachments"],
            serde_json::json!([])
        );
        assert!(arr[2].get("pending_initial_turn").is_none());
    }

    #[test]
    fn missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        fold_pending_initial_turn(&dir.path().join("does-not-exist.json")).unwrap();
    }

    #[test]
    fn unparseable_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(&path, "not json").unwrap();
        fold_pending_initial_turn(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn walks_profile_dirs_and_legacy_root() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("p1");
        fs::create_dir_all(&profile).unwrap();
        let row = r#"[{"id":"a","pending_initial_turn":"go"}]"#;
        fs::write(profile.join("sessions.json"), row).unwrap();
        fs::write(dir.path().join("sessions.json"), row).unwrap();

        run_in(dir.path()).unwrap();

        for p in [
            profile.join("sessions.json"),
            dir.path().join("sessions.json"),
        ] {
            let v: serde_json::Value =
                serde_json::from_str(&fs::read_to_string(&p).unwrap()).unwrap();
            assert_eq!(
                v[0]["pending_initial_turn"]["text"],
                "go",
                "{}",
                p.display()
            );
        }
    }
}
