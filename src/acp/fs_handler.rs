//! Handlers for ACP `fs/*` requests delegated by agents.
//! Every access is opened relative to a session root directory descriptor,
//! never through a symlink, and is logged.

use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use nix::errno::Errno;
use thiserror::Error;
use tracing::info;

use crate::session::AnchoredDir;

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

/// An allowed root opened once, so later renames or symlink swaps of its
/// pathname cannot change which directory requests resolve against.
#[derive(Debug, Clone)]
struct PinnedRoot {
    canonical: PathBuf,
    dir: Option<Arc<AnchoredDir>>,
}

/// Per-session allowed-roots policy.
#[derive(Debug, Clone)]
pub struct FsPolicy {
    pub allowed_roots: Vec<PathBuf>,
    pinned: Vec<PinnedRoot>,
    /// When set, agent-reported paths are translated through this map
    /// before the inside-roots check.
    pub sandbox_map: Option<SandboxPathMap>,
}

impl FsPolicy {
    /// Pins every root that opens now; build the policy before untrusted
    /// code can touch the roots.
    pub fn new(roots: Vec<PathBuf>) -> Self {
        let pinned = roots
            .iter()
            .map(|root| {
                let canonical = root.canonicalize().unwrap_or_else(|_| root.clone());
                let dir = AnchoredDir::open_traversal(&canonical).ok().map(Arc::new);
                PinnedRoot { canonical, dir }
            })
            .collect();
        Self {
            allowed_roots: roots,
            pinned,
            sandbox_map: None,
        }
    }

    pub fn with_map(mut self, map: SandboxPathMap) -> Self {
        self.sandbox_map = Some(map);
        self
    }

    fn translate(&self, path: &Path) -> Result<PathBuf, FsError> {
        if !path.is_absolute() {
            return Err(FsError::NotAbsolute(path.to_path_buf()));
        }
        Ok(match &self.sandbox_map {
            Some(map) => map.translate_to_host(path),
            None => path.to_path_buf(),
        })
    }

    /// Return the pinned root holding `path` and the path relative to it.
    /// Opens walk that relative path with `O_NOFOLLOW` per component, so a
    /// swapped directory cannot redirect them.
    fn anchor(&self, path: &Path) -> Result<(Arc<AnchoredDir>, PathBuf), FsError> {
        let mut normal = PathBuf::new();
        for component in self.translate(path)?.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !normal.pop() {
                        return Err(FsError::OutsideRoots(path.to_path_buf()));
                    }
                }
                other => normal.push(other),
            }
        }
        for (root, pinned) in self.allowed_roots.iter().zip(&self.pinned) {
            for prefix in [root.as_path(), pinned.canonical.as_path()] {
                if let Ok(relative) = normal.strip_prefix(prefix) {
                    let dir = pinned.dir.clone().ok_or_else(|| {
                        FsError::Io(io::Error::new(
                            io::ErrorKind::NotFound,
                            format!("session root was not available: {}", root.display()),
                        ))
                    })?;
                    return Ok((dir, relative.to_path_buf()));
                }
            }
        }
        Err(FsError::OutsideRoots(normal))
    }

    /// Returns the path canonicalized iff it is inside one of the allowed
    /// roots. A check only: it does not protect a later open of the path.
    pub fn resolve_inside(&self, path: &Path) -> Result<PathBuf, FsError> {
        let translated = self.translate(path)?;
        let path = translated.as_path();
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

        for pinned in &self.pinned {
            if canonical.starts_with(&pinned.canonical) {
                return Ok(canonical);
            }
        }
        Err(FsError::OutsideRoots(canonical))
    }
}

fn anchored_error(error: anyhow::Error, path: &Path) -> FsError {
    match error.downcast_ref::<Errno>() {
        Some(Errno::ELOOP) => FsError::SymlinkInPath(path.to_path_buf()),
        Some(errno) => FsError::Io(io::Error::from(*errno)),
        None => FsError::Io(io::Error::other(format!("{error:#}"))),
    }
}

/// Implementation of ACP `fs/read_text_file`.
pub fn handle_read(policy: &FsPolicy, session_id: &str, path: &Path) -> Result<String, FsError> {
    let (root, relative) = policy.anchor(path)?;
    let resolved = root.path().join(&relative);
    let mut text = String::new();
    root.open_file(&relative, false)
        .map_err(|e| anchored_error(e, &resolved))?
        .read_to_string(&mut text)?;
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
    use std::io::Write;
    let (root, relative) = policy.anchor(path)?;
    let resolved = root.path().join(&relative);
    root.open_file(&relative, true)
        .map_err(|e| anchored_error(e, &resolved))?
        .write_all(contents.as_bytes())?;
    info!(
        target: "acp.fs",
        session = %session_id,
        path = %resolved.display(),
        bytes = contents.len(),
        "fs/write"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_inside_accepts_roots_and_mapped_paths_only() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().to_path_buf();
        std::fs::write(root.join("file.txt"), "hello").unwrap();
        let canonical = root.canonicalize().unwrap();
        let map = SandboxPathMap::new(vec![(PathBuf::from("/workspace/proj"), root.clone())]);
        let policy = FsPolicy::new(vec![root.clone()]).with_map(map);
        for inside in [
            root.join("file.txt"),
            PathBuf::from("/workspace/proj/file.txt"),
        ] {
            let resolved = policy
                .resolve_inside(&inside)
                .expect("resolves inside root");
            assert!(resolved.starts_with(&canonical), "{inside:?}");
        }
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

    #[cfg(unix)]
    #[test]
    fn read_write_refuse_symlink_leaves() {
        let temp = tempfile::tempdir().unwrap();
        let policy = FsPolicy::new(vec![temp.path().to_path_buf()]);
        let path = temp.path().join("hello.txt");
        handle_write(&policy, "s-1", &path, "hi there").unwrap();
        assert_eq!(handle_read(&policy, "s-1", &path).unwrap(), "hi there");

        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().join("symlink-target");
        std::fs::write(&outside, "secret").unwrap();
        let escape = temp.path().join("escape");
        std::os::unix::fs::symlink(&outside, &escape).unwrap();
        assert!(matches!(
            handle_read(&policy, "s-1", &escape),
            Err(FsError::SymlinkInPath(_))
        ));
        assert!(matches!(
            handle_write(&policy, "s-1", &escape, "owned"),
            Err(FsError::SymlinkInPath(_))
        ));
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "secret");

        let dangling = temp.path().join("dangling");
        std::os::unix::fs::symlink("/no/such/path", &dangling).unwrap();
        assert!(matches!(
            handle_write(&policy, "s-1", &dangling, "x"),
            Err(FsError::SymlinkInPath(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_refused_even_inside_the_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let policy = FsPolicy::new(vec![root.clone()]);
        std::fs::create_dir(root.join("real")).unwrap();
        std::fs::write(root.join("real/f.txt"), "in").unwrap();
        std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();

        assert!(handle_read(&policy, "s", &root.join("link/f.txt")).is_err());
        assert!(handle_write(&policy, "s", &root.join("link/f.txt"), "x").is_err());
        assert!(handle_write(&policy, "s", &root.join("link/new/f.txt"), "x").is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("real/f.txt")).unwrap(),
            "in"
        );
        assert!(!root.join("real/new").exists());

        handle_write(&policy, "s", &root.join("a/./b/../c/f.txt"), "ok").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a/c/f.txt")).unwrap(),
            "ok"
        );
        assert!(matches!(
            handle_read(&policy, "s", &root.join("../outside.txt")),
            Err(FsError::OutsideRoots(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn replacing_the_root_after_the_policy_is_built_is_not_followed() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let root = parent.join("wt");
        let moved = parent.join("moved");
        let outside = parent.join("outside");
        for dir in [&root, &outside] {
            std::fs::create_dir(dir).unwrap();
        }
        std::fs::write(root.join("f.txt"), "in").unwrap();
        std::fs::write(outside.join("f.txt"), "secret").unwrap();
        let policy = FsPolicy::new(vec![root.clone()]);

        std::fs::rename(&root, &moved).unwrap();
        std::os::unix::fs::symlink(&outside, &root).unwrap();

        assert_eq!(
            handle_read(&policy, "s", &root.join("f.txt")).unwrap(),
            "in"
        );
        handle_write(&policy, "s", &root.join("f.txt"), "new").unwrap();
        handle_write(&policy, "s", &root.join("d/g.txt"), "new").unwrap();
        assert_eq!(std::fs::read_to_string(moved.join("f.txt")).unwrap(), "new");
        assert!(moved.join("d/g.txt").exists());
        assert_eq!(
            std::fs::read_to_string(outside.join("f.txt")).unwrap(),
            "secret"
        );
        assert!(!outside.join("d").exists());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_inside_compares_against_the_pinned_root() {
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().canonicalize().unwrap();
        let (root, outside) = (parent.join("wt"), parent.join("outside"));
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(root.join("f.txt"), "in").unwrap();
        std::fs::write(outside.join("f.txt"), "secret").unwrap();
        let policy = FsPolicy::new(vec![root.clone()]);
        assert!(policy.resolve_inside(&root.join("f.txt")).is_ok());

        std::fs::rename(&root, parent.join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &root).unwrap();

        assert!(matches!(
            policy.resolve_inside(&root.join("f.txt")),
            Err(FsError::OutsideRoots(_))
        ));
    }

    /// Supplemental: a directory flipping to a symlink to an outside
    /// directory while writes run must never redirect a write there.
    #[cfg(unix)]
    #[test]
    fn swapping_a_directory_for_a_symlink_never_escapes_the_root() {
        use std::fs;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        const CYCLES: usize = 20;
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let outside_dir = tempfile::tempdir().unwrap();
        let outside = outside_dir.path().to_path_buf();
        let policy = FsPolicy::new(vec![root.clone()]);
        fs::create_dir(root.join("a")).unwrap();

        // Phase A: no swapper, `a` is a real directory.
        handle_write(&policy, "s", &root.join("a/f.txt"), "in").unwrap();
        handle_write(&policy, "s", &root.join("a/d/f.txt"), "in").unwrap();

        // Phase B: writes race the swapper.
        let done = AtomicBool::new(false);
        let (cycle_tx, cycle_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let (a, a_real) = (root.join("a"), root.join("a_real"));
                while !done.load(Ordering::SeqCst) {
                    let cycle = fs::rename(&a, &a_real)
                        .and_then(|()| std::os::unix::fs::symlink(&outside, &a))
                        .and_then(|()| fs::remove_file(&a))
                        .and_then(|()| fs::rename(&a_real, &a));
                    if cycle.is_ok() {
                        let _ = cycle_tx.send(());
                        continue;
                    }
                    // A writer may have recreated `a` mid-cycle: drop any stray
                    // symlink or writer-made dir so `a_real` moves back.
                    if a.symlink_metadata().is_ok_and(|m| m.is_symlink()) {
                        let _ = fs::remove_file(&a);
                    }
                    if a_real.exists() {
                        let _ = fs::remove_dir_all(&a);
                        let _ = fs::rename(&a_real, &a);
                    }
                    let _ = fs::create_dir(&a);
                }
            });
            let deadline = Instant::now() + Duration::from_secs(30);
            let mut cycles = 0;
            while cycles < CYCLES {
                if Instant::now() > deadline {
                    done.store(true, Ordering::SeqCst);
                    panic!("swapper completed {cycles}/{CYCLES} cycles within 30s");
                }
                while cycle_rx.try_recv().is_ok() {
                    cycles += 1;
                }
                let _ = handle_write(&policy, "s", &root.join("a/f.txt"), "pwn");
                let _ = handle_write(&policy, "s", &root.join("a/d/f.txt"), "pwn");
            }
            done.store(true, Ordering::SeqCst);
        });
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0);
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

    #[cfg(target_os = "linux")]
    #[test]
    fn search_only_root_or_parent_is_readable() {
        use std::os::unix::fs::PermissionsExt;

        struct Restore(PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o700));
            }
        }

        if nix::unistd::geteuid().is_root() {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let parent = temp.path().join("parent");
        let root = parent.join("root");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("f.txt"), "hello").unwrap();
        for search_only in [root.clone(), parent.clone()] {
            let _restore = Restore(search_only.clone());
            std::fs::set_permissions(&search_only, std::fs::Permissions::from_mode(0o100)).unwrap();
            let policy = FsPolicy::new(vec![root.clone()]);
            let text = handle_read(&policy, "s", &root.join("f.txt")).unwrap();
            assert_eq!(text, "hello", "search-only {search_only:?}");
        }
    }
}
