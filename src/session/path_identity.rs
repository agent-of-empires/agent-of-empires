//! Filesystem identities and surviving session references used by cleanup.

use std::path::{Path, PathBuf};

use anyhow::Context;

use super::Instance;

/// Resolve existing ancestors before normalizing a missing suffix.
pub(crate) fn canonicalize_or_raw(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    canonicalize_path(path, false, None)
        .unwrap_or_else(|_| crate::git::template::lexical_normalize(path))
}

fn canonicalize_path(
    path: &Path,
    strict: bool,
    dependencies: Option<&mut Vec<PathIdentity>>,
) -> std::io::Result<PathBuf> {
    let absolute;
    let path = if path.is_relative() {
        absolute = match std::path::absolute(path) {
            Ok(absolute) => absolute,
            Err(error) if strict => return Err(error),
            Err(_) => path.to_owned(),
        };
        absolute.as_path()
    } else {
        path
    };
    if dependencies.is_none() {
        if let Ok(resolved) = std::fs::canonicalize(path) {
            return Ok(resolved);
        }
    }
    let mut resolver = PathResolver {
        resolved: PathBuf::new(),
        strict,
        dependencies,
        remaining_links: 40,
        is_directory: None,
    };
    resolver.visit(path)?;
    Ok(resolver.resolved)
}

struct PathResolver<'a> {
    resolved: PathBuf,
    strict: bool,
    dependencies: Option<&'a mut Vec<PathIdentity>>,
    remaining_links: usize,
    is_directory: Option<bool>,
}

impl PathResolver<'_> {
    fn require_directory(&self) -> std::io::Result<()> {
        if self.strict && self.is_directory == Some(false) {
            return Err(std::io::ErrorKind::NotADirectory.into());
        }
        Ok(())
    }

    fn record_dependency(&mut self) {
        if let Some(paths) = self.dependencies.as_deref_mut() {
            if paths.last().is_some_and(|path| {
                path.lexical == self.resolved && path.traversal.is_none() && path.resolved.is_none()
            }) {
                return;
            }
            paths.push(PathIdentity {
                lexical: self.resolved.clone(),
                traversal: None,
                resolved: None,
            });
        }
    }

    fn visit(&mut self, path: &Path) -> std::io::Result<()> {
        use std::path::Component;
        for component in path.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => {
                    self.resolved.push(component.as_os_str());
                    self.is_directory = None;
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    self.require_directory()?;
                    self.record_dependency();
                    self.resolved.pop();
                    self.is_directory = None;
                }
                Component::Normal(name) => {
                    self.require_directory()?;
                    self.resolved.push(name);
                    match std::fs::symlink_metadata(&self.resolved) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            if self.remaining_links == 0 {
                                return Err(std::io::Error::other("Too many symlink expansions"));
                            }
                            self.remaining_links -= 1;
                            let target = std::fs::read_link(&self.resolved)?;
                            self.record_dependency();
                            self.resolved.pop();
                            self.is_directory = None;
                            self.visit(&target)?;
                        }
                        Ok(metadata) => self.is_directory = Some(metadata.is_dir()),
                        Err(error)
                            if self.strict && error.kind() != std::io::ErrorKind::NotFound =>
                        {
                            return Err(error);
                        }
                        Err(_) => self.is_directory = None,
                    }
                }
            }
        }
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PathIdentity {
    lexical: PathBuf,
    // Components such as link/.. remain necessary to access the destination.
    traversal: Option<PathBuf>,
    resolved: Option<PathBuf>,
}

impl PathIdentity {
    fn new(path: &Path) -> Self {
        let absolute;
        let path = if path.is_relative() {
            absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
            absolute.as_path()
        } else {
            path
        };
        Self::from_resolved(path, canonicalize_or_raw(path))
    }

    fn checked(path: &Path, dependencies: &mut Vec<Self>) -> anyhow::Result<Self> {
        let absolute;
        let path = if path.is_relative() {
            absolute = std::path::absolute(path)?;
            absolute.as_path()
        } else {
            path
        };
        let resolved = canonicalize_path(path, true, Some(dependencies))
            .with_context(|| format!("Cannot resolve resource owner path {}", path.display()))?;
        Ok(Self::from_resolved(path, resolved))
    }

    fn from_resolved(path: &Path, resolved: PathBuf) -> Self {
        let lexical = crate::git::template::lexical_normalize(path);
        Self {
            traversal: (path != lexical).then(|| path.to_owned()),
            resolved: (resolved != lexical).then_some(resolved),
            lexical,
        }
    }

    fn spellings(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.lexical.as_path())
            .chain(self.traversal.as_deref())
            .chain(self.resolved.as_deref())
    }
}

/// Freeze references before cleanup can remove their symlink spellings.
#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct CleanupProtection {
    paths: Vec<PathIdentity>,
    branches: Vec<(usize, String)>,
}

impl CleanupProtection {
    pub(crate) fn validate(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.paths
                .iter()
                .all(|path| path.spellings().all(Path::is_absolute))
                && self
                    .branches
                    .iter()
                    .all(|(index, _)| *index < self.paths.len()),
            "Invalid frozen cleanup references"
        );
        Ok(())
    }

    pub(crate) fn new<'a>(owners: impl IntoIterator<Item = &'a Instance>) -> anyhow::Result<Self> {
        let mut protection = Self::default();
        protection.extend(owners)?;
        Ok(protection)
    }

    pub(crate) fn extend<'a>(
        &mut self,
        owners: impl IntoIterator<Item = &'a Instance>,
    ) -> anyhow::Result<()> {
        for owner in owners {
            self.add_path(&owner.project_path)?;
            if let Some(path) = owner.pre_trash_project_path.as_deref() {
                self.add_path(path)?;
            }
            if let Some(worktree) = &owner.worktree_info {
                self.add_branch(&worktree.main_repo_path, &worktree.branch)?;
            }
            if let Some(workspace) = &owner.workspace_info {
                self.add_path(&workspace.workspace_dir)?;
                for repo in &workspace.repos {
                    self.add_path(&repo.worktree_path)?;
                    self.add_branch(&repo.main_repo_path, &repo.branch)?;
                }
            }
        }
        Ok(())
    }

    fn add_path(&mut self, path: &str) -> anyhow::Result<usize> {
        let path = PathIdentity::checked(Path::new(path), &mut self.paths)?;
        let index = self.paths.len();
        self.paths.push(path);
        Ok(index)
    }

    fn add_branch(&mut self, main_repo: &str, branch: &str) -> anyhow::Result<()> {
        let index = self.add_path(main_repo)?;
        self.branches.push((index, branch.to_owned()));
        Ok(())
    }

    pub(crate) fn references_path(&self, target: &Path) -> bool {
        if self.paths.is_empty() {
            return false;
        }
        let target = PathIdentity::new(target);
        self.paths.iter().any(|path| {
            path.spellings().any(|reference| {
                target
                    .spellings()
                    .any(|target| reference.starts_with(target))
            })
        })
    }

    pub(crate) fn references_ancestor_of(&self, target: &Path) -> bool {
        if self.paths.is_empty() {
            return false;
        }
        let target = PathIdentity::new(target);
        self.paths.iter().any(|path| {
            path.spellings().any(|reference| {
                target
                    .spellings()
                    .any(|target| target.starts_with(reference))
            })
        })
    }

    pub(crate) fn references_branch(&self, main_repo: &Path, branch: &str) -> bool {
        let mut matching = self
            .branches
            .iter()
            .filter(|(_, name)| name == branch)
            .peekable();
        if matching.peek().is_none() {
            return false;
        }
        let main_repo = PathIdentity::new(main_repo);
        matching.any(|(index, _)| {
            self.paths[*index]
                .spellings()
                .any(|reference| main_repo.spellings().any(|target| reference == target))
        })
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn cleanup_keeps_lexical_and_resolved_references_after_paths_disappear() {
        let root = tempfile::tempdir().unwrap();
        let lexical = root.path().join("lexical");
        let real = root.path().join("real");
        let missing_alias = root.path().join("missing-alias");
        let live_alias = root.path().join("live-alias");
        for path in [&lexical, &real.join("live")] {
            std::fs::create_dir_all(path).unwrap();
        }
        std::os::unix::fs::symlink(real.join("live"), lexical.join("link")).unwrap();
        std::os::unix::fs::symlink(real.join("missing"), &missing_alias).unwrap();
        std::os::unix::fs::symlink(real.join("live"), &live_alias).unwrap();
        let mut owners = vec![
            Instance::new("lexical", lexical.join("link").to_str().unwrap()),
            Instance::new("missing", missing_alias.join("child").to_str().unwrap()),
            Instance::new("live", live_alias.to_str().unwrap()),
        ];
        owners[0].pre_trash_project_path =
            Some(real.join("pre-trash").to_string_lossy().into_owned());
        owners[2].worktree_info = Some(crate::session::WorktreeInfo {
            branch: "work".into(),
            main_repo_path: live_alias.to_str().unwrap().into(),
            managed_by_aoe: false,
            created_at: chrono::Utc::now(),
            base_branch: None,
        });
        let protection = CleanupProtection::new(&owners).unwrap();
        assert!(
            protection.references_path(&lexical),
            "recursive cleanup would remove the live access path"
        );
        assert!(
            protection.references_path(&real.join("missing")),
            "missing descendant lost its resolved ancestor"
        );
        assert!(protection.references_path(&real.join("pre-trash")));
        let traversal_owner = Instance::new(
            "traversal",
            lexical.join("missing/../link/../missing").to_str().unwrap(),
        );
        let traversal = CleanupProtection::new([&traversal_owner]).unwrap();
        assert!(
            traversal.references_path(&lexical.join("link")),
            "normalization must not erase a required symlink traversal"
        );
        assert!(traversal.references_path(&real.join("missing")));
        std::fs::remove_file(&live_alias).unwrap();
        std::fs::remove_file(lexical.join("link")).unwrap();
        assert!(
            protection.references_path(&real.join("live")),
            "earlier cleanup must not change the protected identity"
        );
        assert!(
            protection.references_branch(&real.join("live"), "work"),
            "branch ownership lost its frozen repository identity"
        );
    }
}
