//! Preserve unscoped aoe-agent transcripts without guessing historical clear boundaries.
//! The original may be a user artifact; neither it nor existing backups are overwritten.

use anyhow::{bail, Context, Result};
use std::fs;
use std::io::ErrorKind;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let root = app_dir.join("artifacts");
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.file_type().is_symlink() => return Ok(()),
        Ok(_) => {}
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        preserve_transcript(&entry.path())?;
    }
    Ok(())
}

fn preserve_transcript(dir: &Path) -> Result<()> {
    let source = dir.join("transcript.jsonl");
    match fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    }
    let backup = dir.join("transcript.pre-native-id.jsonl");
    match fs::symlink_metadata(&backup) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            // Also finish publication durability after an interrupted earlier run.
            fs::File::open(dir)?.sync_all()?;
            return Ok(());
        }
        Ok(_) => bail!(
            "Legacy transcript backup is not a regular file: {}",
            backup.display()
        ),
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut input = fs::File::open(&source)?;
    let mut staged = tempfile::NamedTempFile::new_in(dir)?;
    std::io::copy(&mut input, &mut staged)?;
    staged.as_file().sync_all()?;
    staged
        .persist_noclobber(&backup)
        .with_context(|| format!("Preserving legacy transcript at {}", backup.display()))?;
    fs::File::open(dir)?.sync_all()?;
    info!(path = %backup.display(), "Preserved legacy transcript; native identity is unknown");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_ambiguous_history_without_assigning_a_native_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("artifacts/instance");
        fs::create_dir_all(&dir).unwrap();
        let legacy = b"{\"role\":\"user\",\"content\":\"before-clear\"}\n{\"role\":\"assistant\",\"content\":\"reply\"}\n";
        fs::write(dir.join("transcript.jsonl"), legacy).unwrap();
        let native = dir.join(format!("aoe-agent-{}.jsonl", "a".repeat(32)));
        fs::write(&native, b"new history").unwrap();

        run_in(tmp.path()).unwrap();
        assert_eq!(fs::read(dir.join("transcript.jsonl")).unwrap(), legacy);
        assert_eq!(
            fs::read(dir.join("transcript.pre-native-id.jsonl")).unwrap(),
            legacy
        );
        assert_eq!(fs::read(&native).unwrap(), b"new history");

        fs::write(dir.join("transcript.jsonl"), b"later user artifact").unwrap();
        run_in(tmp.path()).unwrap();
        assert_eq!(
            fs::read(dir.join("transcript.pre-native-id.jsonl")).unwrap(),
            legacy
        );
        assert_eq!(
            fs::read(dir.join("transcript.jsonl")).unwrap(),
            b"later user artifact"
        );
        assert_eq!(fs::read(&native).unwrap(), b"new history");
    }

    #[test]
    fn refuses_an_obstructed_backup_without_changing_the_original() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("artifacts/instance");
        fs::create_dir_all(dir.join("transcript.pre-native-id.jsonl")).unwrap();
        fs::write(dir.join("transcript.jsonl"), b"only copy").unwrap();
        assert!(run_in(tmp.path()).is_err());
        assert_eq!(
            fs::read(dir.join("transcript.jsonl")).unwrap(),
            b"only copy"
        );
        assert!(dir.join("transcript.pre-native-id.jsonl").is_dir());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_follow_artifact_directory_or_transcript_symlinks() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("transcript.jsonl"), b"outside").unwrap();
        let root = tmp.path().join("artifacts");
        fs::create_dir_all(root.join("regular")).unwrap();
        symlink(outside.path(), root.join("linked-dir")).unwrap();
        symlink(
            outside.path().join("transcript.jsonl"),
            root.join("regular/transcript.jsonl"),
        )
        .unwrap();
        run_in(tmp.path()).unwrap();
        assert!(!outside
            .path()
            .join("transcript.pre-native-id.jsonl")
            .exists());
        assert!(!root.join("regular/transcript.pre-native-id.jsonl").exists());
        assert_eq!(
            fs::read(outside.path().join("transcript.jsonl")).unwrap(),
            b"outside"
        );

        let linked_root = tempfile::tempdir().unwrap();
        symlink(&root, linked_root.path().join("artifacts")).unwrap();
        fs::create_dir_all(root.join("root-check")).unwrap();
        fs::write(
            root.join("root-check/transcript.jsonl"),
            b"preserve in place",
        )
        .unwrap();
        run_in(linked_root.path()).unwrap();
        assert!(!root
            .join("root-check/transcript.pre-native-id.jsonl")
            .exists());
    }
}
