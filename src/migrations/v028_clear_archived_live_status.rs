//! Migration v028: settle the live-interaction status frozen onto archived
//! sessions by builds shipped before `archive()` learned to degrade it.
//!
//! Archiving tears down the session's tmux (#1868), and the status poller
//! deliberately never touches archived rows (#2206), so whatever status the
//! row happened to hold at archive time is persisted verbatim and never
//! revisited. A session archived while `Waiting` (agent blocked on a
//! permission prompt) therefore kept rendering as a pending-permission row
//! in the TUI, `aoe ps`, and the web dashboard indefinitely, with no pane
//! and no process behind it, and no CLI verb able to clear it
//! (`session stop` refuses: the session is not running).
//!
//! This one-shot migration walks every sessions.json and settles any
//! archived row still persisted at a live-interaction status
//! (`running`/`waiting`/`starting`) to `idle`, the same resting state v016
//! chose for archived rows. `archive()` and the poller's archived
//! short-circuit now degrade in-process, so this cleans up only the rows
//! older builds left behind.
//!
//! ## Failure policy
//!
//! Per `AGENTS.md > Data Migrations`, a returned `Err` aborts boot. A
//! sessions.json that fails to parse is logged and skipped (a corrupt file
//! must not block boot or spam every launch). Only `get_app_dir` and
//! directory-read failures propagate.

use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;
use tracing::{debug, info};

/// Migration entry point: settle archived live statuses under the app dir.
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
                clear_archived_live_status(&entry.path().join("sessions.json"))?;
            }
        }
    }
    // Legacy top-level sessions.json (pre-profiles layout).
    clear_archived_live_status(&app_dir.join("sessions.json"))?;
    Ok(())
}

/// Settle any archived session still persisted at a live-interaction status
/// (`running`/`waiting`/`starting`) to `idle`. Leaves non-archived rows and
/// archived rows in any other status untouched.
fn clear_archived_live_status(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("sessions path has no parent: {}", path.display()))?;
    // The lock `Storage::update` holds, kept across read and write so a
    // concurrent update is neither lost nor able to undo the settle.
    let _flock = crate::session::acquire_storage_flock(dir, crate::session::STORAGE_LOCK_FILENAME)?;
    let content = fs::read_to_string(path)?;
    let mut value: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(e) => {
            debug!("v028: failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let mut healed = 0usize;
    if let Some(array) = value.as_array_mut() {
        for instance in array.iter_mut() {
            if let Some(obj) = instance.as_object_mut() {
                let archived = obj.get("archived_at").is_some_and(|v| !v.is_null());
                let live = matches!(
                    obj.get("status").and_then(|v| v.as_str()),
                    Some("running" | "waiting" | "starting")
                );
                if archived && live {
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
            "v028: settled frozen live status on {healed} archived session(s) in {}",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settles_only_archived_live_status_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(
            &path,
            r#"[
                {"id":"a","status":"waiting","archived_at":"2026-07-13T22:17:21Z"},
                {"id":"b","status":"running","archived_at":"2026-07-13T22:17:21Z"},
                {"id":"c","status":"starting","archived_at":"2026-07-13T22:17:21Z"},
                {"id":"d","status":"waiting"},
                {"id":"e","status":"idle","archived_at":"2026-07-13T22:17:21Z"},
                {"id":"f","status":"stopped","archived_at":"2026-07-13T22:17:21Z"},
                {"id":"g","status":"error","archived_at":"2026-07-13T22:17:21Z"},
                {"id":"h","status":"waiting","archived_at":null}
            ]"#,
        )
        .unwrap();

        clear_archived_live_status(&path).unwrap();

        let v: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let arr = v.as_array().unwrap();
        // archived + live-interaction status -> idle (the bug footprint)
        assert_eq!(arr[0]["status"], "idle");
        assert_eq!(arr[1]["status"], "idle");
        assert_eq!(arr[2]["status"], "idle");
        // non-archived waiting -> untouched (a real permission prompt)
        assert_eq!(arr[3]["status"], "waiting");
        // archived resting/terminal statuses -> untouched
        assert_eq!(arr[4]["status"], "idle");
        assert_eq!(arr[5]["status"], "stopped");
        assert_eq!(arr[6]["status"], "error");
        // explicit null archived_at counts as non-archived -> untouched
        assert_eq!(arr[7]["status"], "waiting");
    }

    #[test]
    fn waits_for_an_in_flight_storage_update() {
        use crate::session::{Instance, Status, Storage};
        use std::sync::mpsc;
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        let storage = Storage::new_for_test_path("v028-in-flight", path.clone());
        storage
            .update(|instances, _| {
                let mut frozen = Instance::new("frozen", "/tmp/frozen");
                frozen.archived_at = Some(chrono::Utc::now());
                frozen.status = Status::Waiting;
                instances.push(frozen);
                Ok(())
            })
            .unwrap();

        let (storage, path) = (&storage, &path);
        std::thread::scope(|scope| {
            // Channels live inside the scope so a failed assertion drops
            // `release_tx`, unparks the peer, and the panic propagates.
            let (entered_tx, entered_rx) = mpsc::channel();
            let (release_tx, release_rx) = mpsc::channel::<()>();
            let (done_tx, done_rx) = mpsc::channel();
            // A peer update parked inside the storage lock, mid-commit.
            scope.spawn(move || {
                storage
                    .update(|instances, _| {
                        instances[0].agent_session_id = Some("peer-sid".to_string());
                        entered_tx.send(()).unwrap();
                        release_rx.recv_timeout(Duration::from_secs(10)).unwrap();
                        Ok(())
                    })
                    .unwrap();
            });
            entered_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("peer update never entered the storage lock");
            scope.spawn(move || {
                clear_archived_live_status(path).unwrap();
                done_tx.send(()).unwrap();
            });
            assert!(
                done_rx.recv_timeout(Duration::from_millis(300)).is_err(),
                "migration ran while a storage update was in flight"
            );
            release_tx.send(()).unwrap();
            done_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("migration did not finish after the update committed");
        });

        let loaded = storage.load().unwrap();
        assert_eq!(loaded[0].status, Status::Idle);
        assert_eq!(loaded[0].agent_session_id.as_deref(), Some("peer-sid"));
    }

    #[test]
    fn missing_file_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        clear_archived_live_status(&dir.path().join("does-not-exist.json")).unwrap();
    }

    #[test]
    fn unparseable_file_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sessions.json");
        fs::write(&path, "not json").unwrap();
        clear_archived_live_status(&path).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "not json");
    }

    #[test]
    fn walks_profile_dirs_and_legacy_root() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles").join("p1");
        fs::create_dir_all(&profile).unwrap();
        let row = r#"[{"id":"a","status":"waiting","archived_at":"2026-07-13T22:17:21Z"}]"#;
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
