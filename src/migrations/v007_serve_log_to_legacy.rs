//! Migration v007: rename leftover `serve.log` to `serve.log.legacy`.
//!
//! The serve.log file was retired when foreground and daemon `aoe serve`
//! consolidated onto the configured `[logging].file_path` (debug.log by
//! default). Existing users have a serve.log file from before the upgrade;
//! we rename it to `.legacy` so the bytes aren't lost but `aoe logs` and
//! the TUI dialog no longer try to read it. Idempotent: skips when there
//! is no serve.log to move.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let src = app_dir.join("serve.log");
    if !src.exists() {
        debug!("no serve.log to migrate");
        return Ok(());
    }
    let dst = app_dir.join("serve.log.legacy");
    // Best-effort overwrite: if a prior migration left a `.legacy`, drop it
    // first so the rename can succeed.
    if dst.exists() {
        let _ = fs::remove_file(&dst);
    }
    fs::rename(&src, &dst)?;
    info!(
        target: "migrations",
        from = %src.display(),
        to = %dst.display(),
        "renamed legacy serve.log; logging consolidated under [logging].file_path"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renames_serve_log_to_legacy() {
        let temp = tempfile::tempdir().unwrap();
        let src = temp.path().join("serve.log");
        fs::write(&src, "old daemon output\n").unwrap();

        run_in(temp.path()).unwrap();

        assert!(!src.exists(), "serve.log should be gone");
        let legacy = temp.path().join("serve.log.legacy");
        assert!(legacy.exists(), "serve.log.legacy should exist");
        assert_eq!(fs::read_to_string(&legacy).unwrap(), "old daemon output\n");
    }

    #[test]
    fn noop_when_no_serve_log() {
        let temp = tempfile::tempdir().unwrap();
        run_in(temp.path()).unwrap();
        assert!(!temp.path().join("serve.log.legacy").exists());
    }

    #[test]
    fn idempotent_with_existing_legacy() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(temp.path().join("serve.log"), "new bytes\n").unwrap();
        fs::write(temp.path().join("serve.log.legacy"), "old bytes\n").unwrap();

        run_in(temp.path()).unwrap();

        // Running again with no serve.log present should be a no-op.
        run_in(temp.path()).unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("serve.log.legacy")).unwrap(),
            "new bytes\n"
        );
    }
}
