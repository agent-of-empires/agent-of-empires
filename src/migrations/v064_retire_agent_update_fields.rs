//! Migration v064: drop the fields the retired agent-update restart read. Session rows lose
//! `started_at` and `launch_resolved_executable`. Both are ignored on load, so this only stops
//! dead keys from lingering. Worker records are rewritten at each spawn, so theirs need nothing.

use super::sessions_file;
use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

const RETIRED: [&str; 2] = ["started_at", "launch_resolved_executable"];

pub fn run() -> Result<()> {
    for path in sessions_file::session_files(&crate::session::get_app_dir()?)? {
        drop_retired_fields(&path)?;
    }
    Ok(())
}

fn drop_retired_fields(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _flock = crate::session::acquire_storage_flock(
        path.parent().expect("session file has a parent"),
        crate::session::STORAGE_LOCK_FILENAME,
    )?;
    // An unreadable component keeps its bytes for repair rather than aborting boot.
    let Ok(content) = String::from_utf8(fs::read(path)?) else {
        warn!(target: "migrations.v064", path = %path.display(), "Preserved unreadable session component; its retired fields are ignored once it is repaired");
        return Ok(());
    };
    let dropped = sessions_file::heal_rows(path, &content, |row| {
        RETIRED
            .iter()
            .fold(false, |dropped, key| row.remove(*key).is_some() | dropped)
    })?;
    if dropped > 0 {
        info!(target: "migrations.v064", dropped, path = %path.display(), "dropped retired agent-update fields");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn drops_the_retired_fields_from_rows() {
        let kept = r#"[{"id":"b","launch_executable":"/bin/claude"}]"#;
        assert_rewrites(
            "sessions.json",
            drop_retired_fields,
            &[
                (
                    Some(
                        r#"[{"id":"a","started_at":"2026-01-01T00:00:00Z","launch_executable":"/bin/claude","launch_resolved_executable":"/opt/claude/2.1"},{"id":"b","launch_executable":"/bin/claude"}]"#,
                    ),
                    Some(
                        r#"[{"id":"a","launch_executable":"/bin/claude"},{"id":"b","launch_executable":"/bin/claude"}]"#,
                    ),
                ),
                (Some(kept), Some(kept)),
                (Some("not json"), Some("not json")),
                (None, None),
            ],
        );
    }
}
