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
    fn provisions_a_fresh_dir_under_the_root_and_refuses_reuse() {
        let _tmp = isolate_app_dir();
        let id = format!("test-{}", uuid::Uuid::new_v4());
        let path = provision_scratch_dir(&id).expect("provision must succeed");
        assert!(path.is_dir());
        assert!(path.starts_with(scratch_root().unwrap()));
        assert_eq!(path.file_name().and_then(|n| n.to_str()), Some(id.as_str()));
        assert!(is_scratch_path(&path));
        assert!(
            provision_scratch_dir(&id).is_err(),
            "provision_scratch_dir must error on collision rather than reuse contents",
        );
    }

    #[test]
    #[serial]
    fn is_scratch_path_rejects_paths_outside_the_root() {
        let _tmp = isolate_app_dir();
        let real = provision_scratch_dir(&format!("traverse-{}", uuid::Uuid::new_v4())).unwrap();
        // `real/../..` is the existing app dir: lexically under the root, canonically outside it.
        let traversal = real.join("..").join("..");
        for path in [
            Path::new("/etc"),
            Path::new("/tmp/aoe-scratch-foo"),
            traversal.as_path(),
        ] {
            assert!(!is_scratch_path(path), "{}", path.display());
        }
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
