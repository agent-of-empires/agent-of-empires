//! Legacy rows retain unknown execution coverage until a verified boot change.

use anyhow::Result;
use std::fs;
use std::path::Path;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let unknown =
        serde_json::to_value(crate::session::runner_journal::RunnerExecutionJournal::default())?;
    for path in super::sessions_file::session_files(app_dir)? {
        let Some(directory) = path.parent() else {
            continue;
        };
        let _lock = crate::session::acquire_storage_flock(
            directory,
            crate::session::STORAGE_LOCK_FILENAME,
        )?;
        let content = match fs::read_to_string(&path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        super::sessions_file::heal_rows(&path, &content, |row| {
            if row
                .get("runner_journal")
                .is_some_and(|value| !value.is_null())
            {
                return false;
            }
            row.insert("runner_journal".into(), unknown.clone());
            true
        })?;
        crate::session::sync_parent_directory(&path)?;
    }
    Ok(())
}
