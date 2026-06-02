//! Scratch-session directory provisioning and identification.
//!
//! A scratch session has no associated project path. The session layer
//! provisions a fresh directory under `<app_dir>/scratch/<instance-id>/` and
//! attaches the session to it. On deletion the directory is removed; the
//! "lives under the scratch root" check guards `remove_dir_all` from being
//! aimed at unrelated paths if a session JSON is tampered.
//!
//! Storage under the app dir (instead of `std::env::temp_dir()`) means the
//! directory survives OS temp-dir cleanup (e.g. `systemd-tmpfiles`) and is
//! easy to find when a user wants to peek at the agent's scratch work.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Subdirectory under the app data dir that holds all scratch-session
/// working directories. One child per session, keyed on `Instance.id`.
const SCRATCH_SUBDIR: &str = "scratch";

/// Return the absolute path of the scratch root, creating it lazily.
/// Every scratch session's working directory is provisioned as a child of
/// this directory.
pub fn scratch_root() -> Result<PathBuf> {
    let root = super::get_app_dir()?.join(SCRATCH_SUBDIR);
    if !root.exists() {
        fs::create_dir_all(&root)
            .with_context(|| format!("Failed to create scratch root at {}", root.display()))?;
    }
    Ok(root)
}

/// Create a fresh directory for a scratch session and return its absolute
/// path. Uses `fs::create_dir` (not `create_dir_all`) so a collision with a
/// pre-existing directory surfaces as an error rather than silently reusing
/// the directory's contents, which would violate the freshness contract.
pub fn provision_scratch_dir(instance_id: &str) -> Result<PathBuf> {
    super::validate_instance_id(instance_id)?;
    let path = scratch_root()?.join(instance_id);
    fs::create_dir(&path)
        .with_context(|| format!("Failed to create scratch directory at {}", path.display()))?;
    Ok(path)
}

/// Return true iff `path` is plausibly a scratch directory created by this
/// crate: it lives under `scratch_root()`. Used by
/// `session::deletion::perform_deletion` to guard `fs::remove_dir_all`
/// against accidental or malicious targeting of unrelated paths if a session
/// JSON is hand-edited.
///
/// Both sides are canonicalized before the prefix check so a lexical
/// `..` cannot escape the scratch root (e.g.
/// `<scratch_root>/../profiles` lexically `starts_with(<scratch_root>)`
/// but resolves outside it). A path that does not exist on disk cannot
/// be canonicalized and is refused; that is acceptable because the only
/// caller that needs a yes here is the deletion path, which is removing
/// a directory it just looked up from session state.
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
    use serial_test::serial;

    fn isolate_app_dir() -> tempfile::TempDir {
        // Tests must not write to the user's real app dir. Re-rooting $HOME
        // (or $XDG_CONFIG_HOME on Linux) forces get_app_dir() into a temp.
        let tmp = tempfile::tempdir().expect("create temp home for scratch tests");
        std::env::set_var("HOME", tmp.path());
        #[cfg(target_os = "linux")]
        std::env::set_var("XDG_CONFIG_HOME", tmp.path().join(".config"));
        tmp
    }

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
        // A path that is NOT under scratch_root() must be refused, even if
        // it looks plausible. This is the tampered-JSON defense: a session
        // claiming `scratch: true` with `project_path: /etc` cannot trip the
        // deletion path.
        assert!(!is_scratch_path(Path::new("/etc")));
        assert!(!is_scratch_path(Path::new("/tmp/aoe-scratch-foo")));
    }

    #[test]
    #[serial]
    fn is_scratch_path_rejects_dotdot_traversal() {
        // A lexical `..` inside an otherwise-under-root path used to slip
        // past the guard because `Path::starts_with` is string-based.
        // Canonicalizing both sides resolves the traversal before the
        // prefix check, so `<scratch_root>/<id>/../../etc` must be
        // rejected even though it lexically starts with the root.
        let _tmp = isolate_app_dir();
        let id = format!("traverse-{}", uuid::Uuid::new_v4());
        let real = provision_scratch_dir(&id).unwrap();

        // Build a tampered path that lexically lives under the scratch
        // root but resolves to /etc (a real directory on every host this
        // test runs on, so canonicalize succeeds and lands outside).
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
