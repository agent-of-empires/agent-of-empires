//! Per-session artifact directory provisioning and safe path resolution.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Subdirectory under the app data dir that holds every session's artifact
/// directory. One child per session, keyed on `Instance.id`.
const ARTIFACTS_SUBDIR: &str = "artifacts";

/// Env var pointing the agent at its session artifact directory.
pub const ARTIFACT_DIR_ENV: &str = "AOE_ARTIFACT_DIR";

/// Fixed mount point for the session artifact directory inside a sandbox container.
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

/// Return (creating if needed) the artifact directory for a session.
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

/// Resolve a URL-supplied relative path against a session's artifact directory, returning the
/// canonical file path iff it is a regular file that stays inside the artifact root.
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

/// Path to a session's artifact dir WITHOUT creating it.
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
    fn artifact_dir_is_idempotent_and_resolves_files_under_it() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = session_artifact_dir(&id).unwrap();
        assert_eq!(session_artifact_dir(&id).unwrap(), dir);
        fs::write(dir.join("shot.png"), b"png").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/a.txt"), b"a").unwrap();
        let resolved = resolve_artifact_path(&id, "shot.png").expect("must resolve");
        assert!(resolved.ends_with("shot.png") && resolved.is_file());
        assert!(resolve_artifact_path(&id, "sub/a.txt").is_some());
    }

    #[test]
    #[serial]
    fn resolve_returns_only_regular_files_inside_the_session_dir() {
        let _tmp = isolate_app_dir();
        let id = format!("art-{}", uuid::Uuid::new_v4());
        let dir = session_artifact_dir(&id).unwrap();
        fs::write(dir.join("shot.png"), b"png").unwrap();
        fs::create_dir_all(dir.join("adir")).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/hosts", dir.join("escape")).unwrap();
        // Enough `..` to reach `/` from the session dir, so the traversal row names a real file.
        let traversal = format!("{}etc/hosts", "../".repeat(dir.components().count()));
        // An unsafe id whose base resolves (the app dir) must not reach a file under it.
        let via_parent = format!("artifacts/{id}/shot.png");
        for (instance, rel) in [
            (id.as_str(), traversal.as_str()),
            (id.as_str(), "escape"),
            (id.as_str(), "nope.png"),
            (id.as_str(), "adir"),
            ("..", via_parent.as_str()),
        ] {
            assert_eq!(
                resolve_artifact_path(instance, rel),
                None,
                "{instance} {rel}"
            );
        }
    }
}
