//! Scratch-session directory provisioning and identification.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Subdirectory under the app data dir that holds all scratch-session
/// working directories. One child per session, keyed on `Instance.id`.
const SCRATCH_SUBDIR: &str = "scratch";

/// Return the absolute path of the scratch root, creating it lazily.
pub fn scratch_root() -> Result<PathBuf> {
    let root = super::get_app_dir()?.join(SCRATCH_SUBDIR);
    if !root.exists() {
        fs::create_dir_all(&root)
            .with_context(|| format!("Failed to create scratch root at {}", root.display()))?;
    }
    Ok(root)
}

/// Create a fresh directory for a scratch session and return its absolute path.
pub fn provision_scratch_dir(instance_id: &str) -> Result<PathBuf> {
    super::validate_instance_id(instance_id)?;
    let path = scratch_root()?.join(instance_id);
    fs::create_dir(&path)
        .with_context(|| format!("Failed to create scratch directory at {}", path.display()))?;
    Ok(path)
}

/// Return true iff `path` is plausibly a scratch directory created by this crate: it lives under
/// `scratch_root()`.
pub fn is_scratch_path(path: &Path) -> bool {
    let Ok(root) = scratch_root() else {
        return false;
    };
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let Ok(path) = path.canonicalize() else {
        return false;
    };
    path.starts_with(&root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::isolate_app_dir;
    use serial_test::serial;

    #[test]
    #[serial]
    fn provisions_and_returns_app_dir_path() {
        let _tmp = isolate_app_dir();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let path = provision_scratch_dir(&id).expect("provision must succeed");
        assert!(path.exists());
        assert!(path.is_dir());
        assert!(path.starts_with(scratch_root().unwrap()));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(id.as_str()));
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    #[serial]
    fn provision_collision_errors() {
        let _tmp = isolate_app_dir();
        let id = format!("collision-{}", uuid::Uuid::new_v4());
        let first = provision_scratch_dir(&id).expect("first provision must succeed");
        let second = provision_scratch_dir(&id);
        assert!(
            second.is_err(),
            "provision_scratch_dir must error on collision rather than reuse contents",
        );
        let _ = fs::remove_dir_all(&first);
    }

    #[test]
    #[serial]
    fn is_scratch_path_accepts_under_root() {
        let _tmp = isolate_app_dir();
        let id = format!("guard-accept-{}", uuid::Uuid::new_v4());
        let path = provision_scratch_dir(&id).unwrap();
        assert!(is_scratch_path(&path));
        let _ = fs::remove_dir_all(&path);
    }

    #[test]
    #[serial]
    fn is_scratch_path_rejects_outside_root() {
        let _tmp = isolate_app_dir();
        assert!(!is_scratch_path(Path::new("/etc")));
        assert!(!is_scratch_path(Path::new("/tmp/aoe-scratch-foo")));
    }

    #[test]
    #[serial]
    fn is_scratch_path_rejects_dotdot_traversal() {
        let _tmp = isolate_app_dir();
        let id = format!("traverse-{}", uuid::Uuid::new_v4());
        let real = provision_scratch_dir(&id).unwrap();

        let tampered = real.join("..").join("..").join("..").join("etc");
        assert!(
            !is_scratch_path(&tampered),
            "`..` traversal must not escape the scratch root"
        );

        let _ = fs::remove_dir_all(&real);
    }

    #[test]
    #[serial]
    fn provision_scratch_dir_rejects_unsafe_id() {
        let _tmp = isolate_app_dir();
        assert!(provision_scratch_dir("../etc").is_err());
        assert!(provision_scratch_dir("foo bar").is_err());
        assert!(provision_scratch_dir("").is_err());
    }
}
