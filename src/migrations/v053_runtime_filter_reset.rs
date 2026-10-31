//! Migration v053: delete the persisted `runtime_filter`, which predates the
//! `warn` catch-all and the new target roots; boot re-derives it from config.

use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    remove_filter(&crate::logging::runtime_filter_path(&app_dir))
}

fn remove_filter(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {
            info!(target: "migrations.v053", "Removed stale runtime log filter {}", path.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::remove_filter;

    #[test]
    fn removes_the_filter_once_and_tolerates_absence() {
        for present in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("runtime_filter");
            if present {
                std::fs::write(&path, "agent_of_empires=info").unwrap();
            }
            for run in 0..2 {
                remove_filter(&path).unwrap();
                assert!(!path.exists(), "present {present} run {run}");
            }
        }
    }
}
