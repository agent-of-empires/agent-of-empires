//! Handlers for ACP `fs/*` requests delegated by agents.
//! Every access resolves (following symlinks) inside the session roots and is logged.

use std::io;
use std::path::{Path, PathBuf};

use thiserror::Error;
use tracing::info;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("path is outside session roots: {0}")]
    OutsideRoots(PathBuf),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("path is not absolute: {0}")]
    NotAbsolute(PathBuf),
    #[error("path contains invalid utf-8")]
    NonUtf8Path,
    #[error("refusing to write through symlink: {0}")]
    SymlinkInPath(PathBuf),
}

/// Host↔container path translation for sandboxed sessions.
#[derive(Debug, Clone, Default)]
pub struct SandboxPathMap {
    pub mounts: Vec<(PathBuf, PathBuf)>,
}

impl SandboxPathMap {
    pub fn new(mounts: Vec<(PathBuf, PathBuf)>) -> Self {
        Self { mounts }
    }

    /// If `path` is rooted at one of the container-side prefixes,
    /// rewrite it to the matching host-side prefix.
    pub fn translate_to_host(&self, path: &Path) -> PathBuf {
        let mut best: Option<(&Path, &Path)> = None;
        for (container, host) in &self.mounts {
            if path.starts_with(container)
                && best
                    .map(|(c, _)| container.as_os_str().len() > c.as_os_str().len())
                    .unwrap_or(true)
            {
                best = Some((container, host));
            }
        }
        match best {
            Some((container, host)) => {
                let rel = path
                    .strip_prefix(container)
                    .unwrap_or_else(|_| Path::new(""));
                if rel.as_os_str().is_empty() {
                    host.to_path_buf()
                } else {
                    host.join(rel)
                }
            }
            None => path.to_path_buf(),
        }
    }
}

/// Per-session allowed-roots policy.
#[derive(Debug, Clone)]
pub struct FsPolicy {
    pub allowed_roots: Vec<PathBuf>,
    /// When set, agent-reported paths are translated through this map
    /// before the inside-roots check.
    pub sandbox_map: Option<SandboxPathMap>,
}

impl FsPolicy {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            allowed_roots: roots,
            sandbox_map: None,
        }
    }

    pub fn with_sandbox_map(roots: Vec<PathBuf>, map: SandboxPathMap) -> Self {
        Self {
            allowed_roots: roots,
            sandbox_map: Some(map),
        }
    }

    /// Returns the path canonicalized iff it is inside one of the allowed
    /// roots.
    pub fn resolve_inside(&self, path: &Path) -> Result<PathBuf, FsError> {
        if !path.is_absolute() {
            return Err(FsError::NotAbsolute(path.to_path_buf()));
        }
        let translated: PathBuf;
        let path = if let Some(map) = &self.sandbox_map {
            translated = map.translate_to_host(path);
            translated.as_path()
        } else {
            path
        };
        let canonical = if path.exists() {
            path.canonicalize()?
        } else if let Some(parent) = path.parent() {
            let parent_canonical = parent.canonicalize()?;
            match path.file_name() {
                Some(name) => parent_canonical.join(name),
                None => parent_canonical,
            }
        } else {
            path.to_path_buf()
        };

        // If the leaf exists as a symlink (even when the symlink target
        // doesn't), reject.
        if let Ok(meta) = std::fs::symlink_metadata(&canonical) {
            if meta.file_type().is_symlink() {
                return Err(FsError::SymlinkInPath(canonical));
            }
        }

        for root in &self.allowed_roots {
            let root_canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
            if canonical.starts_with(&root_canonical) {
                return Ok(canonical);
            }
        }
        Err(FsError::OutsideRoots(canonical))
    }
}

/// Implementation of ACP `fs/read_text_file`.
pub fn handle_read(policy: &FsPolicy, session_id: &str, path: &Path) -> Result<String, FsError> {
    let resolved = policy.resolve_inside(path)?;
    let text = read_no_follow(&resolved)?;
    info!(target: "acp.fs", session = %session_id, path = %resolved.display(), bytes = text.len(), "fs/read");
    Ok(text)
}

/// Implementation of ACP `fs/write_text_file`.
pub fn handle_write(
    policy: &FsPolicy,
    session_id: &str,
    path: &Path,
    contents: &str,
) -> Result<(), FsError> {
    let resolved = policy.resolve_inside(path)?;
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent)?;
    }
    write_no_follow(&resolved, contents)?;
    info!(
        target: "acp.fs",
        session = %session_id,
        path = %resolved.display(),
        bytes = contents.len(),
        "fs/write"
    );
    Ok(())
}

/// Open with `O_NOFOLLOW` so the kernel itself refuses to follow a
/// symlink at the leaf.
#[cfg(unix)]
fn read_no_follow(path: &Path) -> io::Result<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok(buf)
}

#[cfg(not(unix))]
fn read_no_follow(path: &Path) -> io::Result<String> {
    std::fs::read_to_string(path)
}

#[cfg(unix)]
fn write_no_follow(path: &Path, contents: &str) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .custom_flags(nix::fcntl::OFlag::O_NOFOLLOW.bits())
        .open(path)?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

#[cfg(not(unix))]
fn write_no_follow(path: &Path, contents: &str) -> io::Result<()> {
    std::fs::write(path, contents)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn resolve_inside_allowed_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        fs::write(root.join("file.txt"), "hello").unwrap();
        let policy = FsPolicy::new(vec![root.clone()]);
        let resolved = policy
            .resolve_inside(&root.join("file.txt"))
            .expect("should resolve inside root");
        assert!(resolved.starts_with(root.canonicalize().unwrap()));
    }

    #[test]
    fn resolve_inside_rejects_relative_and_out_of_root_paths() {
        let temp = tempfile::tempdir().unwrap();
        let policy = FsPolicy::new(vec![temp.path().to_path_buf()]);
        let outside = std::env::temp_dir().join("definitely-not-in-temp-dir-of-test");
        assert!(matches!(
            policy.resolve_inside(&outside),
            Err(FsError::OutsideRoots(_))
        ));
        assert!(matches!(
            policy.resolve_inside(Path::new("relative/file.txt")),
            Err(FsError::NotAbsolute(_))
        ));
    }

    #[test]
    fn read_and_write_roundtrip() {
        let temp = tempfile::tempdir().unwrap();
        let policy = FsPolicy::new(vec![temp.path().to_path_buf()]);
        let path = temp.path().join("hello.txt");
        handle_write(&policy, "s-1", &path, "hi there").unwrap();
        let read = handle_read(&policy, "s-1", &path).unwrap();
        assert_eq!(read, "hi there");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_leaf_pointing_outside_root() {
        let temp = tempfile::tempdir().unwrap();
        let policy = FsPolicy::new(vec![temp.path().to_path_buf()]);
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("symlink-target");
        std::fs::write(&outside, "secret").unwrap();
        let symlink_in_root = temp.path().join("escape");
        std::os::unix::fs::symlink(&outside, &symlink_in_root).unwrap();

        let read_result = handle_read(&policy, "s-1", &symlink_in_root);
        assert!(matches!(read_result, Err(FsError::OutsideRoots(_))));

        let write_result = handle_write(&policy, "s-1", &symlink_in_root, "owned");
        assert!(matches!(write_result, Err(FsError::OutsideRoots(_))));

        let target_after = std::fs::read_to_string(&outside).unwrap();
        assert_eq!(target_after, "secret", "outside file must remain untouched");
    }

    /// Dangling symlink (target does not exist) inside the allowed root.
    #[cfg(unix)]
    #[test]
    fn rejects_dangling_symlink_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let policy = FsPolicy::new(vec![temp.path().to_path_buf()]);
        let dangling = temp.path().join("dangling");
        std::os::unix::fs::symlink("/no/such/path", &dangling).unwrap();
        let result = handle_write(&policy, "s-1", &dangling, "x");
        assert!(matches!(result, Err(FsError::SymlinkInPath(_))));
    }

    #[test]
    fn sandbox_path_map_translates_on_the_longest_matching_mount() {
        let map = SandboxPathMap::new(vec![
            (PathBuf::from("/workspace"), PathBuf::from("/Users/me/all")),
            (
                PathBuf::from("/workspace/proj"),
                PathBuf::from("/Users/me/proj"),
            ),
        ]);
        // (container path, host path)
        let cases = [
            ("/workspace/proj/src/main.rs", "/Users/me/proj/src/main.rs"),
            ("/workspace/other/x", "/Users/me/all/other/x"),
            // Unmatched paths pass through untouched.
            ("/etc/hosts", "/etc/hosts"),
        ];
        for (container, host) in cases {
            assert_eq!(
                map.translate_to_host(Path::new(container)),
                PathBuf::from(host),
                "{container}"
            );
        }
    }

    #[test]
    fn fs_policy_resolves_container_path_via_sandbox_map() {
        let temp = tempfile::tempdir().unwrap();
        let host_root = temp.path().to_path_buf();
        std::fs::write(host_root.join("file.txt"), "ok").unwrap();
        let map = SandboxPathMap::new(vec![(PathBuf::from("/workspace/proj"), host_root.clone())]);
        let policy = FsPolicy::with_sandbox_map(vec![host_root.clone()], map);
        let resolved = policy
            .resolve_inside(Path::new("/workspace/proj/file.txt"))
            .expect("should resolve via sandbox map");
        assert!(resolved.starts_with(host_root.canonicalize().unwrap()));
    }

    #[cfg(unix)]
    #[test]
    fn open_with_nofollow_rejects_symlink_leaf() {
        let temp = tempfile::tempdir().unwrap();
        let target = temp.path().join("real");
        std::fs::write(&target, "ok").unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            read_no_follow(&link).is_err(),
            "O_NOFOLLOW must refuse a symlinked leaf"
        );
    }
}
