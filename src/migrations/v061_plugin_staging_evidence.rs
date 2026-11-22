//! Preserve legacy staging evidence without manufacturing process ownership.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

use crate::plugin::install::{OPERATION_LOCK, RECOVERY_MARKER, STAGING_MARKER};
use crate::session::AnchoredDir;

const LEGACY_PREFIX: &[u8] =
    b"Plugin staging acquisition started; verify the volume and its writers before removing this marker.\nmount=";
const MAX_EVIDENCE_BYTES: usize = 1024 * 1024;

pub fn run() -> Result<()> {
    migrate_root(&crate::session::get_app_dir()?)
}

fn migrate_root(path: &Path) -> Result<()> {
    let root = AnchoredDir::open(path)?;
    let marker = Path::new(STAGING_MARKER);
    if root.entry_stat(marker)?.is_none() {
        return Ok(());
    }
    let bytes = root
        .read_regular(marker, MAX_EVIDENCE_BYTES)?
        .context("plugin staging evidence is not a bounded regular file")?;
    if !bytes.starts_with(LEGACY_PREFIX) {
        return Ok(());
    }
    let _operation = crate::session::try_acquire_storage_flock(path, OPERATION_LOCK)?
        .context("cannot migrate staging evidence while a plugin operation is active")?;
    anyhow::ensure!(
        root.read_regular(marker, MAX_EVIDENCE_BYTES)?.as_deref() == Some(bytes.as_slice()),
        "plugin staging evidence changed before migration"
    );
    let recovery = Path::new(RECOVERY_MARKER);
    if let Some(mut file) = root.create_new_regular(recovery)? {
        file.write_all(
            b"Legacy plugin staging evidence has no process-birth or native resource ownership proof. Preserve .plugin-staging-active and verify its resources before manual recovery.\n",
        )?;
        file.sync_all()?;
        root.sync()?;
    } else {
        root.open_regular(recovery, MAX_EVIDENCE_BYTES)?
            .context("existing plugin recovery evidence is not a bounded regular file")?;
    }
    tracing::info!(target: "migrations.v061", "Preserved legacy plugin staging evidence; explicit ownership recovery remains required");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    #[serial_test::serial]
    fn legacy_upgrade_preserves_resources_and_blocks_new_operations() {
        for prior_recovery in [None, Some(b"retained operator evidence\n".as_slice())] {
            let _home = crate::session::test_support::isolate_app_dir();
            let app = crate::session::get_app_dir().unwrap();
            let stage = app.join("plugins/.staging-legacy/volume");
            fs::create_dir_all(&stage).unwrap();
            fs::write(stage.join("retained"), b"unknown owner data").unwrap();
            let mut legacy = LEGACY_PREFIX.to_vec();
            legacy.extend_from_slice(stage.to_str().unwrap().as_bytes());
            legacy.push(b'\n');
            fs::write(app.join(STAGING_MARKER), &legacy).unwrap();
            if let Some(prior) = prior_recovery {
                fs::write(app.join(RECOVERY_MARKER), prior).unwrap();
            }

            migrate_root(&app).unwrap();
            let recovery = fs::read(app.join(RECOVERY_MARKER)).unwrap();
            assert!(crate::plugin::install::begin_operation().is_err());
            assert_eq!(fs::read(app.join(STAGING_MARKER)).unwrap(), legacy);
            assert_eq!(
                fs::read(stage.join("retained")).unwrap(),
                b"unknown owner data"
            );
            if let Some(prior) = prior_recovery {
                assert_eq!(recovery, prior);
            }
            migrate_root(&app).unwrap();
            assert_eq!(fs::read(app.join(RECOVERY_MARKER)).unwrap(), recovery);
            assert_eq!(fs::read(app.join(STAGING_MARKER)).unwrap(), legacy);
        }
    }

    #[test]
    fn active_operation_prevents_legacy_evidence_migration() {
        let temp = tempfile::tempdir().unwrap();
        let legacy = [LEGACY_PREFIX, b"/unproven/stage\n"].concat();
        fs::write(temp.path().join(STAGING_MARKER), &legacy).unwrap();
        let _operation = crate::session::try_acquire_storage_flock(temp.path(), OPERATION_LOCK)
            .unwrap()
            .unwrap();
        assert!(migrate_root(temp.path()).is_err());
        assert_eq!(fs::read(temp.path().join(STAGING_MARKER)).unwrap(), legacy);
        assert!(!temp.path().join(RECOVERY_MARKER).exists());
    }

    #[test]
    fn current_or_unrecognized_evidence_is_not_rewritten() {
        let temp = tempfile::tempdir().unwrap();
        for evidence in [b"{\"schema\":1}\n".as_slice(), b"unrecognized evidence\n"] {
            fs::write(temp.path().join(STAGING_MARKER), evidence).unwrap();
            migrate_root(temp.path()).unwrap();
            assert_eq!(
                fs::read(temp.path().join(STAGING_MARKER)).unwrap(),
                evidence
            );
            assert!(!temp.path().join(RECOVERY_MARKER).exists());
        }
    }

    #[cfg(unix)]
    #[test]
    fn linked_evidence_never_changes_its_target() {
        let temp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let target = outside.path().join("user-data");
        let legacy = [LEGACY_PREFIX, b"/unproven/stage\n"].concat();
        fs::write(&target, &legacy).unwrap();
        std::os::unix::fs::symlink(&target, temp.path().join(STAGING_MARKER)).unwrap();
        assert!(migrate_root(temp.path()).is_err());
        assert_eq!(fs::read(&target).unwrap(), legacy);
        assert!(!temp.path().join(RECOVERY_MARKER).exists());
    }
}
