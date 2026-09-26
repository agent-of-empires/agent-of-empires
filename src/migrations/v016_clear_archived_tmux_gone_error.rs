//! Migration v016: demote archived sessions still persisted at
//! `status = "error"` back to Idle.
//!
//! Builds between #1868 and #2206 archived a session, killed its tmux, then
//! let the poller stamp the missing tmux as an error. `last_error` is not
//! persisted, so the row survives as a bare Error. An archived row has no
//! live tmux by design, so that Error can only be the spurious transition.
//! A sessions.json that fails to parse is logged and skipped.

use super::sessions_file;
use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    for path in sessions_file::session_files(app_dir)? {
        clear_archived_error(&path)?;
    }
    Ok(())
}

/// Demote any archived row still persisted at `status = "error"` back to Idle,
/// leaving non-archived rows and every other status alone.
fn clear_archived_error(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let healed = sessions_file::heal_rows(path, &fs::read_to_string(path)?, |row| {
        let spurious =
            sessions_file::is_archived(row) && sessions_file::status(row) == Some("error");
        if spurious {
            sessions_file::settle_to_idle(row);
        }
        spurious
    })?;
    if healed > 0 {
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
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn clears_only_archived_error_rows() {
        let corrupt = "{ not valid json";
        assert_rewrites(
            "sessions.json",
            clear_archived_error,
            &[
                (
                    Some(
                        r#"[
                {"id":"a","status":"error","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"b","status":"error"},
                {"id":"c","status":"idle","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"d","status":"stopped","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"e","status":"error","archived_at":null}
            ]"#,
                    ),
                    // Only archived + error (the bug footprint) settles; a null
                    // archived_at counts as not archived.
                    Some(
                        r#"[
                {"id":"a","status":"idle","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"b","status":"error"},
                {"id":"c","status":"idle","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"d","status":"stopped","archived_at":"2026-06-05T16:04:12Z"},
                {"id":"e","status":"error","archived_at":null}
            ]"#,
                    ),
                ),
                (Some(corrupt), Some(corrupt)),
                (None, None),
            ],
        );
    }
}
