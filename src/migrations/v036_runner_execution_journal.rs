//! Legacy rows retain unknown execution coverage until a verified boot change.

use anyhow::Result;
use std::fs;
use std::path::Path;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    tracing::info!(target: "migrations", "v036: establishing execution coverage");
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn legacy_coverage_is_unknown_and_existing_journals_are_preserved() {
        let temporary = tempfile::tempdir().unwrap();
        let directory = temporary.path().join("profiles/default");
        fs::create_dir_all(&directory).unwrap();
        let path = directory.join("sessions.json");
        let history = serde_json::json!({"coverage":"complete", "launches":[{"nonce":[1], "sentinel":"keep"}]});
        let pending = serde_json::json!({"coverage":"complete", "launches":[], "preparations":[{"nonce":[2], "generation":7}]});
        fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!([
                {"id":"legacy", "sentinel":"retain"},
                {"id":"history", "runner_journal":history},
                {"id":"pending", "runner_journal":pending}
            ]))
            .unwrap(),
        )
        .unwrap();
        run_in(temporary.path()).unwrap();
        let migrated: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(migrated[0]["sentinel"], "retain");
        assert_eq!(migrated[0]["runner_journal"]["coverage"], "unknown");
        assert_eq!(
            migrated[0]["runner_journal"]["preparations"],
            serde_json::json!([])
        );
        assert_eq!(
            migrated[1]["runner_journal"]["launches"],
            history["launches"]
        );
        assert_eq!(migrated[1]["runner_journal"], history);
        assert_eq!(migrated[2]["runner_journal"], pending);
        let bytes = fs::read(&path).unwrap();
        run_in(temporary.path()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
