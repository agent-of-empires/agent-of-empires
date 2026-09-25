//! Migration v028: settle archived sessions still persisted at a
//! live-interaction status (`running`, `waiting`, `starting`) to Idle, the
//! resting state v016 chose.
//!
//! Archiving tears down tmux and the poller never revisits archived rows, so
//! older builds persisted whatever status held at archive time; a row archived
//! while Waiting rendered as a pending permission prompt forever with nothing
//! behind it. `archive()` and the poller now degrade in-process. A
//! sessions.json that fails to parse is logged and skipped.

use super::sessions_file;
use anyhow::{anyhow, Result};
use std::fs;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

/// Walk every profile's `sessions.json` plus the legacy top-level one. Split
/// from `run` so tests can point it at a temp dir.
pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    for path in sessions_file::session_files(app_dir)? {
        clear_archived_live_status(&path)?;
    }
    Ok(())
}

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
    let healed = sessions_file::heal_rows(path, &fs::read_to_string(path)?, |row| {
        let frozen = sessions_file::is_archived(row)
            && matches!(
                sessions_file::status(row),
                Some("running" | "waiting" | "starting")
            );
        if frozen {
            sessions_file::settle_to_idle(row);
        }
        frozen
    })?;
    if healed > 0 {
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
            let (contended_tx, contended_rx) = mpsc::channel();
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
                let _observer = crate::session::observe_lock_contention_for_test(contended_tx);
                clear_archived_live_status(path).unwrap();
                done_tx.send(()).unwrap();
            });
            let contention = contended_rx.recv_timeout(Duration::from_secs(5));
            let completed_while_held = done_rx.try_recv();
            release_tx.send(()).unwrap();
            assert_eq!(
                contention.expect("migration must contend on the real storage lock"),
                path.parent()
                    .unwrap()
                    .join(crate::session::STORAGE_LOCK_FILENAME)
            );
            assert!(
                matches!(completed_while_held, Err(mpsc::TryRecvError::Empty)),
                "migration completed before the holder released"
            );
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
