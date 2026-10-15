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
    fn moves_serve_log_aside_as_legacy() {
        // (serve.log, serve.log.legacy) before -> legacy after; serve.log is always gone.
        for (log, legacy, expected) in [
            (
                Some("old daemon output\n"),
                None,
                Some("old daemon output\n"),
            ),
            (None, None, None),
            (
                Some("new bytes\n"),
                Some("old bytes\n"),
                Some("new bytes\n"),
            ),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (log_path, legacy_path) = (
                temp.path().join("serve.log"),
                temp.path().join("serve.log.legacy"),
            );
            for (path, content) in [(&log_path, log), (&legacy_path, legacy)] {
                if let Some(content) = content {
                    fs::write(path, content).unwrap();
                }
            }
            // A second run, with no serve.log left, changes nothing.
            for _ in 0..2 {
                run_in(temp.path()).unwrap();
                assert!(!log_path.exists(), "{log:?}");
                assert_eq!(fs::read_to_string(&legacy_path).ok().as_deref(), expected);
            }
        }
    }
}
