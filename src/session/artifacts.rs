//! Per-session artifact directory provisioning and safe path resolution.
//!
//! Agents generate files a user wants to view in the dashboard (screenshots,
//! status HTML). Historically they wrote these to arbitrary `/tmp` paths of
//! their own choosing, which the web backend cannot serve: serving a path
//! chosen by the (untrusted) agent output would be a local-file-inclusion
//! hole, and in a Docker sandbox the path is not even reachable from the host
//! serve process.
//!
//! Instead we give every session an aoe-managed artifact directory under
//! `<app_dir>/artifacts/<instance-id>/`, exported to the agent via the
//! `AOE_ARTIFACT_DIR` env var (and bind-mounted to [`CONTAINER_ARTIFACT_DIR`]
//! inside a sandbox). Only files under that directory are ever served, and
//! [`resolve_artifact_path`] canonicalizes before a prefix check so neither a
//! lexical `..` nor a symlink can escape the root. See #2587.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Subdirectory under the app data dir that holds every session's artifact
/// directory. One child per session, keyed on `Instance.id`.
const ARTIFACTS_SUBDIR: &str = "artifacts";

/// Env var pointing the agent at its session artifact directory. On the host
/// this is the absolute `<app_dir>/artifacts/<id>` path; inside a sandbox it
/// is [`CONTAINER_ARTIFACT_DIR`], which bind-mounts back to that host dir.
pub const ARTIFACT_DIR_ENV: &str = "AOE_ARTIFACT_DIR";

/// Fixed mount point for the session artifact directory inside a sandbox
/// container. The host `<app_dir>/artifacts/<id>` dir is bind-mounted here so
/// artifacts an agent writes in the container land in the served host dir.
pub const CONTAINER_ARTIFACT_DIR: &str = "/aoe/artifacts";

/// Return the absolute path of the artifacts root, creating it lazily.
fn artifacts_root() -> Result<PathBuf> {
    let root = super::get_app_dir()?.join(ARTIFACTS_SUBDIR);
    if !root.exists() {
        fs::create_dir_all(&root)
            .with_context(|| format!("Failed to create artifacts root at {}", root.display()))?;
    }
    Ok(root)
}

/// Return (creating if needed) the artifact directory for a session. Unlike
/// scratch dirs this is reused across restarts, so `create_dir_all` is
/// intentional: the directory persists for the life of the session.
pub fn session_artifact_dir(instance_id: &str) -> Result<PathBuf> {
    super::validate_instance_id(instance_id)?;
    let path = artifacts_root()?.join(instance_id);
    if !path.exists() {
        fs::create_dir_all(&path).with_context(|| {
            format!("Failed to create artifact directory at {}", path.display())
        })?;
    }
    Ok(path)
}

/// Resolve a URL-supplied relative path against a session's artifact
/// directory, returning the canonical file path iff it is a regular file that
/// stays inside the artifact root. Returns `None` for traversal attempts,
/// symlink escapes, non-existent paths, and non-file targets.
///
/// Both the root and the candidate are canonicalized before the prefix check,
/// so a lexical `..` or an in-dir symlink pointing outside the root cannot
/// escape: the resolved target simply fails `starts_with(root)`.
pub fn resolve_artifact_path(instance_id: &str, rel: &str) -> Option<PathBuf> {
    if super::validate_instance_id(instance_id).is_err() {
        return None;
    }
    let base = artifacts_root().ok()?.join(instance_id);
    let root = base.canonicalize().ok()?;
    let candidate = base.join(rel.trim_start_matches('/'));
    let resolved = candidate.canonicalize().ok()?;
    if resolved.starts_with(&root) && is_regular_file(&resolved) {
        Some(resolved)
    } else {
        None
    }
}

/// Path to a session's artifact dir WITHOUT creating it. For read-only
/// surfaces (e.g. the session API response) that must not provision anything.
/// Returns `None` when the id is unsafe or the app dir cannot be resolved.
pub fn artifact_dir_path(instance_id: &str) -> Option<PathBuf> {
    if super::validate_instance_id(instance_id).is_err() {
        return None;
    }
    Some(
        super::get_app_dir()
            .ok()?
            .join(ARTIFACTS_SUBDIR)
            .join(instance_id),
    )
}

fn is_regular_file(path: &Path) -> bool {
    fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::test_support::isolate_app_dir;
    use serial_test::serial;

    #[test]
    #[serial]
    fn session_artifact_dir_is_idempotent() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let first = session_artifact_dir(&id).expect("first must succeed");
        let second = session_artifact_dir(&id).expect("second must succeed");
        assert_eq!(first, second);
        assert!(first.is_dir());
    }

    #[test]
    #[serial]
    fn resolve_accepts_regular_file_under_root() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = session_artifact_dir(&id).unwrap();
        fs::write(dir.join("shot.png"), b"png").unwrap();
        let resolved = resolve_artifact_path(&id, "shot.png").expect("must resolve");
        assert!(resolved.ends_with("shot.png"));
        assert!(resolved.is_file());
    }

    #[test]
    #[serial]
    fn resolve_accepts_nested_file() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = session_artifact_dir(&id).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/a.txt"), b"a").unwrap();
        assert!(resolve_artifact_path(&id, "sub/a.txt").is_some());
    }

    #[test]
    #[serial]
    fn resolve_rejects_dotdot_traversal() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        session_artifact_dir(&id).unwrap();
        // Resolves to /etc/hosts, outside the artifact root.
        assert!(resolve_artifact_path(&id, "../../../../etc/hosts").is_none());
    }

    #[test]
    #[serial]
    fn resolve_rejects_symlink_escape() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = session_artifact_dir(&id).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc/hosts", dir.join("escape")).unwrap();
            assert!(resolve_artifact_path(&id, "escape").is_none());
        }
    }

    #[test]
    #[serial]
    fn resolve_rejects_missing_and_non_file() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = session_artifact_dir(&id).unwrap();
        assert!(resolve_artifact_path(&id, "nope.png").is_none());
        fs::create_dir_all(dir.join("adir")).unwrap();
        assert!(resolve_artifact_path(&id, "adir").is_none());
    }

    #[test]
    #[serial]
    fn resolve_rejects_unsafe_instance_id() {
        let _tmp = isolate_app_dir();
        assert!(resolve_artifact_path("../etc", "hosts").is_none());
    }
}
