//! Producer-held pre-effect resources for the original Create transaction.
use super::{AnchoredDir, DirectoryIdentity, Instance};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

#[cfg(test)]
thread_local! {
    pub(crate) static FAIL_BRANCH_RETIRE_ONCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct DirectoryPin {
    directory: AnchoredDir,
    birth: DirectoryIdentity,
    mode: u32,
    parent_undo_leaf: Option<String>,
}
impl DirectoryPin {
    fn capture(path: &Path) -> Result<Self> {
        let directory = AnchoredDir::open(path)?;
        let birth = directory.birth_identity()?;
        let mode = directory.permissions_mode()?;
        Ok(Self {
            directory,
            birth,
            mode,
            parent_undo_leaf: None,
        })
    }
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            self.birth.is_durable(),
            "original directory birth is unavailable"
        );
        let current = AnchoredDir::open(self.directory.path())?;
        anyhow::ensure!(
            current.birth_identity()? == self.birth
                && self.directory.birth_identity()? == self.birth
                && self.directory.permissions_mode()? == self.mode
                && current.permissions_mode()? == self.mode,
            "original directory was replaced: {}",
            self.directory.path().display()
        );
        Ok(())
    }
}

struct PathPlan {
    path: PathBuf,
    parent: DirectoryPin,
    missing: Vec<PathBuf>,
    preexisting: bool,
    produced: Option<DirectoryPin>,
    tree: Option<Tree>,
    tree_uncertainty: Option<String>,
    removed: bool,
    undo_leaf: String,
    consuming: bool,
}
impl PathPlan {
    fn freeze(path: &Path) -> Result<Self> {
        anyhow::ensure!(path.is_absolute(), "creation resource must be absolute");
        let mut cursor = path.to_path_buf();
        let mut missing = Vec::new();
        loop {
            match std::fs::symlink_metadata(&cursor) {
                Ok(metadata) => {
                    anyhow::ensure!(
                        metadata.is_dir() && !metadata.file_type().is_symlink(),
                        "creation physical parent is not a directory: {}",
                        cursor.display()
                    );
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    missing.push(cursor.clone());
                    cursor = cursor
                        .parent()
                        .context("creation path has no physical parent")?
                        .to_path_buf();
                }
                Err(error) => return Err(error.into()),
            }
        }
        let preexisting = missing.is_empty();
        missing.reverse();
        Ok(Self {
            path: path.to_path_buf(),
            parent: DirectoryPin::capture(&cursor)?,
            missing,
            preexisting,
            produced: None,
            tree: None,
            tree_uncertainty: None,
            removed: false,
            undo_leaf: format!(".aoe-create-undo-{}", uuid::Uuid::new_v4()),
            consuming: false,
        })
    }
    fn validate_before_effect(&self) -> Result<()> {
        self.parent.validate()?;
        if !self.preexisting && self.produced.is_none() {
            anyhow::ensure!(
                !self.path.try_exists()?,
                "creation destination acquired by another producer"
            );
        }
        Ok(())
    }
    fn acknowledge_mkdir(&mut self, original: &DirectoryPin) -> Result<()> {
        self.parent.validate()?;
        anyhow::ensure!(
            !self.preexisting && original.directory.path() == self.path,
            "directory acknowledgement is not the original admitted mkdir result"
        );
        original.validate()?;
        if let Some(produced) = &self.produced {
            produced.validate()?;
            anyhow::ensure!(
                produced.birth == original.birth,
                "original mkdir birth changed"
            );
        } else {
            // Duplicate the retained producer PFD, never reopen a post-effect path
            // and turn somebody else's replacement into our creation capability.
            let directory = original.directory.child(Path::new(""))?;
            anyhow::ensure!(
                directory.birth_identity()? == original.birth,
                "producer PFD changed"
            );
            self.produced = Some(DirectoryPin {
                directory,
                birth: original.birth,
                mode: original.mode,
                parent_undo_leaf: None,
            });
        }
        Ok(())
    }
}

// Bytes are committed canonically in a bounded streaming pass. Only the
// parent/root PFD is retained; per-file birth and mode are checked through
// temporary no-follow descriptors anchored at that physical parent.
struct Tree {
    birth: DirectoryIdentity,
    mode: u32,
    entries: Vec<(std::ffi::OsString, Entry)>,
}
enum Entry {
    Directory(Tree),
    File {
        birth: DirectoryIdentity,
        mode: u32,
        commitment: FileCommitment,
    },
}
struct FileCommitment {
    salt: [u8; 16],
    length: u64,
    digest: [u8; 32],
}
impl FileCommitment {
    fn stream(file: &mut std::fs::File, salt: [u8; 16], length: u64) -> Result<Self> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        let mut hasher = Sha256::new();
        hasher.update(b"aoe-original-create-undo-file-v1\0");
        hasher.update(salt);
        hasher.update(length.to_le_bytes());
        let mut total = 0u64;
        let mut buffer = [0u8; 8192];
        loop {
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            total = total
                .checked_add(read as u64)
                .context("file commitment length overflow")?;
            anyhow::ensure!(total <= length, "created file grew during commitment");
            hasher.update(&buffer[..read]);
        }
        anyhow::ensure!(
            total == length,
            "created file changed length during commitment"
        );
        Ok(Self {
            salt,
            length,
            digest: hasher.finalize().into(),
        })
    }
}
impl Tree {
    fn capture(dir: &AnchoredDir) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let birth = dir.birth_identity()?;
        let mode = dir.permissions_mode()?;
        anyhow::ensure!(birth.is_durable(), "created directory birth is unavailable");
        let mut entries = Vec::new();
        for name in dir.read_dir(Path::new(""), usize::MAX)? {
            let path = Path::new(&name);
            let entry = if let Ok(child) = dir.child(path) {
                Entry::Directory(Self::capture(&child)?)
            } else {
                let mut file = dir
                    .open_regular(path, usize::MAX)?
                    .context("symlink/special-file effect remains protected")?;
                let before = file.metadata()?;
                let birth = DirectoryIdentity::from_metadata(&before);
                anyhow::ensure!(birth.is_durable(), "created file birth is unavailable");
                let mode = before.permissions().mode();
                let commitment = FileCommitment::stream(
                    &mut file,
                    *uuid::Uuid::new_v4().as_bytes(),
                    before.len(),
                )?;
                let after = file.metadata()?;
                anyhow::ensure!(
                    DirectoryIdentity::from_metadata(&after) == birth
                        && after.permissions().mode() == mode
                        && after.len() == before.len()
                        && after.modified().ok() == before.modified().ok(),
                    "created file changed while its effect result was committed"
                );
                Entry::File {
                    birth,
                    mode,
                    commitment,
                }
                // Temporary file descriptor closes here, not at custodian retirement.
            };
            entries.push((name, entry));
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        anyhow::ensure!(
            dir.birth_identity()? == birth,
            "created directory changed during commitment"
        );
        anyhow::ensure!(
            dir.permissions_mode()? == mode,
            "created directory permissions changed during commitment"
        );
        Ok(Self {
            birth,
            mode,
            entries,
        })
    }
    fn consume(&mut self, dir: &AnchoredDir) -> Result<()> {
        self.validate(dir)?;
        while let Some((name, entry)) = self.entries.last_mut() {
            let path = Path::new(name);
            match entry {
                Entry::Directory(tree) => {
                    let child = dir.child(path)?;
                    tree.consume(&child)?;
                    anyhow::ensure!(
                        child.birth_identity()? == tree.birth,
                        "emptied original directory changed birth"
                    );
                    dir.remove_empty_child(path)?;
                }
                Entry::File {
                    birth,
                    mode,
                    commitment,
                } => {
                    let proof = ProtectedDirty {
                        path: path.to_path_buf(),
                        proof: Some(Entry::File {
                            birth: *birth,
                            mode: *mode,
                            commitment: FileCommitment {
                                salt: commitment.salt,
                                length: commitment.length,
                                digest: commitment.digest,
                            },
                        }),
                    };
                    proof.validate(dir)?;
                    dir.remove_original_file(path)?;
                }
            }
            self.entries.pop(); // Commit the actual unlink ACK before fallible durability work.
            dir.sync()?;
        }
        dir.sync() // Also retries durability when every prior unlink is already ACKed.
    }
    fn validate_seed(&self, dir: &AnchoredDir) -> Result<()> {
        anyhow::ensure!(
            dir.birth_identity()? == self.birth && dir.permissions_mode()? == self.mode,
            "original Git admin root changed birth/mode"
        );
        for (name, entry) in &self.entries {
            match entry {
                Entry::Directory(tree) => tree.validate(&dir.child(Path::new(name))?)?,
                Entry::File {
                    birth,
                    mode,
                    commitment,
                } => {
                    let proof = ProtectedDirty {
                        path: PathBuf::from(name),
                        proof: Some(Entry::File {
                            birth: *birth,
                            mode: *mode,
                            commitment: FileCommitment {
                                salt: commitment.salt,
                                length: commitment.length,
                                digest: commitment.digest,
                            },
                        }),
                    };
                    proof.validate(dir)?;
                }
            }
        }
        Ok(())
    }
    fn validate(&self, dir: &AnchoredDir) -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        anyhow::ensure!(
            dir.birth_identity()? == self.birth && dir.permissions_mode()? == self.mode,
            "created tree changed birth/mode"
        );
        let mut names = dir.read_dir(Path::new(""), usize::MAX)?;
        names.sort();
        anyhow::ensure!(
            names.len() == self.entries.len()
                && names
                    .iter()
                    .zip(&self.entries)
                    .all(|(name, (expected, _))| name == expected),
            "created tree contains new or removed data"
        );
        for (name, entry) in &self.entries {
            let path = Path::new(name);
            match entry {
                Entry::Directory(tree) => tree.validate(&dir.child(path)?)?,
                Entry::File {
                    birth,
                    mode,
                    commitment,
                } => {
                    let mut file = dir
                        .open_regular(path, usize::MAX)?
                        .context("created file changed type")?;
                    let before = file.metadata()?;
                    anyhow::ensure!(
                        DirectoryIdentity::from_metadata(&before) == *birth,
                        "created file was replaced"
                    );
                    anyhow::ensure!(
                        before.permissions().mode() == *mode,
                        "created file permissions changed"
                    );
                    anyhow::ensure!(
                        before.len() == commitment.length,
                        "created file changed length"
                    );
                    let actual =
                        FileCommitment::stream(&mut file, commitment.salt, commitment.length)?;
                    let after = file.metadata()?;
                    anyhow::ensure!(
                        actual.digest == commitment.digest
                            && DirectoryIdentity::from_metadata(&after) == *birth
                            && after.permissions().mode() == *mode
                            && after.len() == commitment.length
                            && after.modified().ok() == before.modified().ok(),
                        "created file bytes/identity/permissions changed"
                    );
                }
            }
        }
        anyhow::ensure!(
            dir.birth_identity()? == self.birth,
            "created tree changed during validation"
        );
        Ok(())
    }
}

fn checkout_statuses(repository: &git2::Repository) -> Result<git2::Statuses<'_>> {
    let mut options = git2::StatusOptions::new();
    options
        .include_untracked(true)
        .recurse_untracked_dirs(true)
        .include_ignored(true)
        .recurse_ignored_dirs(true);
    Ok(repository.statuses(Some(&mut options))?)
}
fn clean_checkout(repository: &git2::Repository) -> Result<bool> {
    Ok(checkout_statuses(repository)?.is_empty())
}

fn common_directory(repository: &git2::Repository) -> Result<PathBuf> {
    if repository.is_worktree() {
        let relative = std::fs::read_to_string(repository.path().join("commondir"))?;
        Ok(repository.path().join(relative.trim()).canonicalize()?)
    } else {
        Ok(repository.path().canonicalize()?)
    }
}

pub(crate) struct OwnedWorktreeLayout {
    pub(crate) root: super::builder::AnchoredDir,
    pub(crate) admin: super::builder::AnchoredDir,
    pub(crate) common: super::builder::AnchoredDir,
    pub(crate) oid: git2::Oid,
}
struct GitAdmin {
    pin: DirectoryPin,
    seed: Tree,
    result: Option<Tree>,
    consuming: bool,
}
struct FrozenGitFile {
    path: PathBuf,
    birth: DirectoryIdentity,
    mode: u32,
    bytes: Vec<u8>,
}
impl FrozenGitFile {
    fn read(directory: &AnchoredDir, path: &Path) -> Result<Option<Self>> {
        use std::io::Read;
        use std::os::unix::fs::PermissionsExt;
        let Some(mut file) = directory.open_regular(path, usize::MAX)? else {
            anyhow::ensure!(
                directory.entry_identity(path)?.is_none(),
                "Git bootstrap source is not a regular file"
            );
            return Ok(None);
        };
        let metadata = file.metadata()?;
        let birth = DirectoryIdentity::from_metadata(&metadata);
        anyhow::ensure!(birth.is_durable(), "Git bootstrap file birth unavailable");
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        anyhow::ensure!(
            DirectoryIdentity::from_metadata(&file.metadata()?) == birth
                && metadata.len() == bytes.len() as u64,
            "Git bootstrap source changed during freeze"
        );
        Ok(Some(Self {
            path: path.to_path_buf(),
            birth,
            mode: metadata.permissions().mode(),
            bytes,
        }))
    }
    fn validate(&self, directory: &AnchoredDir) -> Result<()> {
        let current =
            Self::read(directory, &self.path)?.context("Git bootstrap source disappeared")?;
        anyhow::ensure!(
            current.birth == self.birth && current.mode == self.mode && current.bytes == self.bytes,
            "original Git bootstrap source changed"
        );
        Ok(())
    }
}
struct ProtectedDirty {
    path: PathBuf,
    proof: Option<Entry>,
}
impl ProtectedDirty {
    fn freeze(root: &AnchoredDir, path: PathBuf) -> Result<Self> {
        let proof = if root.entry_identity(&path)?.is_none() {
            None
        } else if let Ok(directory) = root.child(&path) {
            Some(Entry::Directory(Tree::capture(&directory)?))
        } else {
            use std::os::unix::fs::PermissionsExt;
            let mut file = root
                .open_regular(&path, usize::MAX)?
                .context("dirty symlink/special-file remains protected")?;
            let metadata = file.metadata()?;
            let birth = DirectoryIdentity::from_metadata(&metadata);
            anyhow::ensure!(birth.is_durable(), "dirty source file birth unavailable");
            let commitment = FileCommitment::stream(
                &mut file,
                *uuid::Uuid::new_v4().as_bytes(),
                metadata.len(),
            )?;
            let after = file.metadata()?;
            anyhow::ensure!(
                DirectoryIdentity::from_metadata(&after) == birth
                    && after.len() == metadata.len()
                    && after.permissions().mode() == metadata.permissions().mode()
                    && after.modified().ok() == metadata.modified().ok(),
                "dirty source changed while frozen"
            );
            Some(Entry::File {
                birth,
                mode: metadata.permissions().mode(),
                commitment,
            })
        };
        Ok(Self { path, proof })
    }
    fn validate(&self, root: &AnchoredDir) -> Result<()> {
        match &self.proof {
            None => anyhow::ensure!(
                root.entry_identity(&self.path)?.is_none(),
                "original deleted source path acquired data"
            ),
            Some(Entry::Directory(tree)) => tree.validate(&root.child(&self.path)?)?,
            Some(Entry::File {
                birth,
                mode,
                commitment,
            }) => {
                use std::os::unix::fs::PermissionsExt;
                let mut file = root
                    .open_regular(&self.path, usize::MAX)?
                    .context("dirty source file changed type")?;
                let metadata = file.metadata()?;
                anyhow::ensure!(
                    DirectoryIdentity::from_metadata(&metadata) == *birth
                        && metadata.permissions().mode() == *mode
                        && metadata.len() == commitment.length,
                    "dirty source file changed identity/mode/length"
                );
                anyhow::ensure!(
                    FileCommitment::stream(&mut file, commitment.salt, commitment.length)?.digest
                        == commitment.digest,
                    "dirty source file bytes changed"
                );
            }
        }
        Ok(())
    }
}
fn tracking_values(config: &git2::Config, key: &str) -> Result<Vec<String>> {
    let mut entries = match config.multivar(key, None) {
        Ok(entries) => entries,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut values = Vec::new();
    while let Some(entry) = entries.next() {
        let entry = entry?;
        if entry.include_depth() != 0 {
            continue;
        }
        anyhow::ensure!(entry.has_value(), "valueless tracking config requires a raw Git-config value producer and remains protected: {key}");
        values.push(entry.value()?.to_owned());
    }
    Ok(values)
}
struct TrackingUndo {
    remote: String,
    merge: String,
    removed: [bool; 2],
    restored: [usize; 2],
}
struct GitPlan {
    repo: PathBuf,
    common: DirectoryPin,
    branch: String,
    path: PathBuf,
    before: Option<git2::Oid>,
    produced: Option<git2::Oid>,
    preexisting_worktrees: Vec<String>,
    before_refs: Vec<(String, Option<git2::Oid>)>,
    before_dirty: Vec<(Vec<u8>, git2::Status)>,
    source: DirectoryPin,
    source_git: DirectoryPin,
    worktrees_parent: Option<DirectoryPin>,
    admin_name: String,
    admin_undo_leaf: String,
    layout_removed: bool,
    branch_deleted: bool,
    admin: Option<GitAdmin>,
    config_source: Option<FrozenGitFile>,
    sparse_source: Option<FrozenGitFile>,
    protected_dirty: Vec<ProtectedDirty>,
    checkout_started: bool,
    bootstrap_complete: bool,
    submodule_uncertainty: Option<String>,
    tracking: Option<TrackingUndo>,
    tracking_uncertainty: Option<String>,
    before_tracking: (Vec<String>, Vec<String>),
    add_outcome: Option<bool>,
    admin_uncertainty: Option<String>,
    checkout_acknowledged: bool,
    clean: bool,
    removed: bool,
}

pub(crate) struct CreationUndo {
    paths: Vec<PathPlan>,
    directories: Vec<DirectoryPin>,
    pending_sync: Vec<AnchoredDir>,
    git: Vec<GitPlan>,
    // This records the intended actual domain before daemon interaction, not
    // a CLI PID or a post-hoc container-name lookup masquerading as custody.
    container: Option<(String, String, String)>,
    container_goals: Vec<Vec<String>>,
    container_results: Vec<String>,
}
impl CreationUndo {
    pub(crate) fn freeze(instance: &Instance, paths: &[PathBuf]) -> Result<Self> {
        let mut seen = std::collections::HashSet::new();
        let mut paths = paths
            .iter()
            .filter(|path| seen.insert((*path).clone()))
            .map(|path| PathPlan::freeze(path))
            .collect::<Result<Vec<_>>>()?;
        if paths.is_empty() && !instance.project_path.is_empty() {
            paths.push(PathPlan::freeze(Path::new(&instance.project_path))?);
        }
        let mut git = Vec::new();
        let mut repos: Vec<_> = instance
            .all_repos()
            .iter()
            .filter(|repo| repo.managed_by_aoe)
            .map(|repo| {
                (
                    repo.main_repo_path.clone(),
                    repo.branch.clone(),
                    repo.worktree_path.clone(),
                )
            })
            .collect();
        if let Some(info) = instance
            .worktree_info
            .as_ref()
            .filter(|info| info.managed_by_aoe)
        {
            repos.push((
                info.main_repo_path.clone(),
                info.branch.clone(),
                instance.project_path.clone(),
            ));
        }
        for (main_repo_path, branch, worktree_path) in repos {
            let repository = crate::git::open_repo_at(Path::new(&main_repo_path))?;
            let before = repository
                .find_reference(&format!("refs/heads/{}", branch))
                .ok()
                .and_then(|r| r.target());
            let before_refs = repository
                .references()?
                .map(|r| {
                    let r = r?;
                    Ok((
                        r.name()
                            .context("non-UTF8 reference remains protected")?
                            .to_owned(),
                        r.target(),
                    ))
                })
                .collect::<Result<Vec<_>>>()?;
            let before_dirty: Vec<(Vec<u8>, git2::Status)> = if repository.is_bare() {
                Vec::new()
            } else {
                checkout_statuses(&repository)?
                    .iter()
                    .map(|entry| (entry.path_bytes().to_vec(), entry.status()))
                    .collect()
            };
            let common = DirectoryPin::capture(common_directory(&repository)?.as_path())?;
            let source = DirectoryPin::capture(&Path::new(&main_repo_path).canonicalize()?)?;
            let source_git = DirectoryPin::capture(&repository.path().canonicalize()?)?;
            let worktrees_parent = match common.directory.child(Path::new("worktrees")) {
                Ok(directory) => {
                    let birth = directory.birth_identity()?;
                    Some(DirectoryPin {
                        mode: directory.permissions_mode()?,
                        directory,
                        birth,
                        parent_undo_leaf: None,
                    })
                }
                Err(error) => {
                    anyhow::ensure!(
                        common
                            .directory
                            .entry_identity(Path::new("worktrees"))?
                            .is_none(),
                        "Git worktrees parent is not an original directory: {error:#}"
                    );
                    None
                }
            };
            let config = repository.config()?;
            let local_config = git2::Config::open(&common.directory.path().join("config"))?;
            let before_tracking = (
                tracking_values(&local_config, &format!("branch.{branch}.remote"))?,
                tracking_values(&local_config, &format!("branch.{branch}.merge"))?,
            );
            let config_source = if config
                .get_bool("extensions.worktreeConfig")
                .unwrap_or(false)
            {
                FrozenGitFile::read(&source_git.directory, Path::new("config.worktree"))?
            } else {
                None
            };
            let sparse_source = if config.get_bool("core.sparseCheckout").unwrap_or(false) {
                FrozenGitFile::read(&source_git.directory, Path::new("info/sparse-checkout"))?
            } else {
                None
            };
            let protected_dirty = before_dirty
                .iter()
                .map(|(name, _)| {
                    use std::os::unix::ffi::OsStringExt;
                    ProtectedDirty::freeze(
                        &source.directory,
                        PathBuf::from(std::ffi::OsString::from_vec(name.clone())),
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            git.push(GitPlan {
                repo: PathBuf::from(&main_repo_path),
                common,
                branch: branch.clone(),
                path: PathBuf::from(&worktree_path),
                before,
                produced: None,
                preexisting_worktrees: repository
                    .worktrees()?
                    .iter()
                    .map(|name| {
                        name?
                            .context("non-UTF8 protected worktree name")
                            .map(str::to_owned)
                    })
                    .collect::<Result<Vec<_>>>()?,
                before_refs,
                before_dirty,
                source,
                source_git,
                worktrees_parent,
                admin_name: format!("aoe-{}", uuid::Uuid::new_v4()),
                admin_undo_leaf: format!(".aoe-admin-undo-{}", uuid::Uuid::new_v4()),
                layout_removed: false,
                branch_deleted: false,
                admin: None,
                config_source,
                sparse_source,
                protected_dirty,
                checkout_started: false,
                bootstrap_complete: false,
                submodule_uncertainty: None,
                tracking: None,
                tracking_uncertainty: None,
                before_tracking,
                add_outcome: None,
                admin_uncertainty: None,
                checkout_acknowledged: false,
                clean: true,
                removed: false,
            });
        }
        Ok(Self {
            paths,
            directories: Vec::new(),
            pending_sync: Vec::new(),
            git,
            container: None,
            container_goals: Vec::new(),
            container_results: Vec::new(),
        })
    }
    pub(crate) fn require_worktree(
        &mut self,
        repo: &Path,
        branch: &str,
        path: &Path,
    ) -> Result<()> {
        let plan = self
            .git
            .iter()
            .find(|plan| plan.repo == repo && plan.branch == branch && plan.path == path)
            .context("Git invocation has no original pre-effect plan")?;
        plan.common.validate()?;
        plan.source.validate()?;
        plan.source_git.validate()?;
        let repository = crate::git::open_repo_at(repo)?;
        let actual_common = DirectoryPin::capture(&common_directory(&repository)?)?;
        anyhow::ensure!(
            actual_common.birth == plan.common.birth,
            "Git common directory changed before effect"
        );
        let path = self
            .paths
            .iter()
            .find(|plan| plan.path == path)
            .context("worktree path was not reserved")?;
        path.validate_before_effect()?;
        if let Some(root) = &path.produced {
            root.validate()?;
        }
        let parent = path
            .path
            .parent()
            .context("worktree has no parent")?
            .to_path_buf();
        self.provision_parents(&parent)
    }
    fn provision_parents(&mut self, path: &Path) -> Result<()> {
        let plan = self
            .paths
            .iter()
            .find(|plan| path == plan.path || plan.path.starts_with(path))
            .context("directory is outside the pre-effect plan")?;
        plan.parent.validate()?;
        let missing: Vec<_> = plan
            .missing
            .iter()
            .filter(|component| *component == path || path.starts_with(component))
            .cloned()
            .collect();
        for component in missing {
            if let Some(pin) = self
                .directories
                .iter()
                .find(|pin| pin.directory.path() == component)
            {
                pin.validate()?;
                continue;
            }
            let parent_path = component.parent().context("directory has no parent")?;
            if let Some(original) = self
                .directories
                .iter()
                .find(|pin| pin.directory.path() == parent_path)
            {
                original.validate()?;
            } else {
                plan.parent.validate()?;
                anyhow::ensure!(
                    parent_path == plan.parent.directory.path(),
                    "mkdir parent is not an original admitted directory"
                );
            }
            let parent = AnchoredDir::open(parent_path)?;
            let child = parent.create_fresh_child(Path::new(
                component.file_name().context("directory has no leaf")?,
            ))?;
            let birth = child.birth_identity()?;
            self.directories.push(DirectoryPin {
                mode: child.permissions_mode()?,
                directory: child,
                birth,
                parent_undo_leaf: None,
            });
        }
        Ok(())
    }
    pub(crate) fn provision(&mut self, path: &Path) -> Result<()> {
        let index = self
            .paths
            .iter()
            .position(|plan| plan.path == path)
            .context("directory not in original plan")?;
        self.paths[index].validate_before_effect()?;
        anyhow::ensure!(
            !self.paths[index].preexisting,
            "preexisting directory is protected"
        );
        self.provision_parents(path)?;
        let original = self
            .directories
            .iter()
            .find(|pin| pin.directory.path() == path)
            .context("no retained original mkdir producer result for this resource")?;
        self.paths[index].acknowledge_mkdir(original)?;
        match self.paths[index]
            .produced
            .as_ref()
            .map(|pin| Tree::capture(&pin.directory))
        {
            Some(Ok(tree)) => {
                self.paths[index].tree = Some(tree);
                self.paths[index].tree_uncertainty = None;
            }
            Some(Err(error)) => {
                self.paths[index].tree = None;
                self.paths[index].tree_uncertainty = Some(format!("{error:#}"));
            }
            None => {}
        }
        Ok(())
    }
    pub(crate) fn bind_command_directory(
        &self,
        intent: &super::builder::CreationIntent,
        command: &mut super::runner_journal::OwnedCreateCommand,
        cwd: &Path,
        git: bool,
    ) -> Result<()> {
        let token = |pin: &DirectoryPin| -> Result<super::builder::AnchoredDir> {
            pin.validate()?;
            super::builder::AnchoredDir::from_original(intent, &pin.directory, pin.birth)
        };
        if let Some(plan) = self
            .git
            .iter()
            .find(|plan| plan.repo == cwd || plan.path == cwd)
        {
            plan.common.validate()?;
            if let Some(admin) = &plan.admin {
                admin.pin.validate()?;
            }
            if let Some(root) = self
                .paths
                .iter()
                .find(|resource| resource.path == plan.path)
                .and_then(|resource| resource.produced.as_ref())
            {
                root.validate()?;
            }
            if cwd == plan.repo {
                command.anchored_current_dir(&token(&plan.source)?)?;
                if git {
                    command.anchored_env_path("GIT_DIR", &token(&plan.source_git)?)?;
                    command.anchored_env_path("GIT_COMMON_DIR", &token(&plan.common)?)?;
                    let repository = crate::git::open_repo_at(&plan.repo)?;
                    if repository.is_bare() {
                        command.env_remove("GIT_WORK_TREE");
                    } else {
                        command.anchored_env_path("GIT_WORK_TREE", &token(&plan.source)?)?;
                    }
                }
            } else {
                let resource = self
                    .paths
                    .iter()
                    .find(|resource| resource.path == cwd)
                    .context("original checkout path missing")?;
                resource.parent.validate()?;
                let root = resource
                    .produced
                    .as_ref()
                    .context("checkout root has no original producer birth")?;
                command.anchored_current_dir(&token(root)?)?;
                if git {
                    let admin = plan
                        .admin
                        .as_ref()
                        .context("checkout admin has no original producer birth")?;
                    command.anchored_env_path("GIT_DIR", &token(&admin.pin)?)?;
                    command.anchored_env_path("GIT_COMMON_DIR", &token(&plan.common)?)?;
                    command.anchored_env_path("GIT_WORK_TREE", &token(root)?)?;
                }
            }
            return Ok(());
        }
        anyhow::ensure!(
            !git,
            "Git directory is outside its original frozen resource plan"
        );
        let resource = self
            .paths
            .iter()
            .find(|resource| resource.path == cwd)
            .context("hook cwd is outside its original frozen resource plan")?;
        resource.parent.validate()?;
        let pin = if resource.preexisting {
            &resource.parent
        } else {
            resource
                .produced
                .as_ref()
                .context("hook cwd has no original producer birth")?
        };
        command.anchored_current_dir(&token(pin)?)?;
        Ok(())
    }

    fn allocate_git_file(directory: &AnchoredDir, path: &Path, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        let mut file = directory
            .create_new_regular(path)?
            .context("Git layout entry acquired by another producer")?;
        let birth = DirectoryIdentity::from_metadata(&file.metadata()?);
        anyhow::ensure!(birth.is_durable(), "new Git layout file birth unavailable");
        file.write_all(bytes)?;
        file.sync_all()?;
        let mut read = directory
            .open_regular(path, usize::MAX)?
            .context("Git layout entry disappeared")?;
        anyhow::ensure!(
            DirectoryIdentity::from_metadata(&read.metadata()?) == birth,
            "our actual Git layout file was replaced before ACK"
        );
        let actual = FileCommitment::stream(&mut read, [0; 16], bytes.len() as u64)?;
        use sha2::{Digest, Sha256};
        let mut expected = Sha256::new();
        expected.update(b"aoe-original-create-undo-file-v1\0");
        expected.update([0; 16]);
        expected.update((bytes.len() as u64).to_le_bytes());
        expected.update(bytes);
        anyhow::ensure!(
            actual.digest == <[u8; 32]>::from(expected.finalize()),
            "Git layout write did not produce its intended bytes"
        );
        directory.sync()
    }

    pub(crate) fn allocate_worktree_bootstrap(
        &mut self,
        repo: &Path,
        branch: &str,
        path: &Path,
        lock_reason: &str,
    ) -> Result<()> {
        self.require_worktree(repo, branch, path)?;
        let index = self
            .git
            .iter()
            .position(|plan| plan.repo == repo && plan.branch == branch && plan.path == path)
            .context("checkout effect is outside its original pre-effect plan")?;
        anyhow::ensure!(
            self.git[index].admin.is_none(),
            "original checkout producer was already allocated"
        );
        self.provision(path)?;
        let resource = self
            .paths
            .iter_mut()
            .find(|plan| plan.path == path)
            .context("original path disappeared")?;
        let root = resource
            .produced
            .as_ref()
            .context("checkout root has no original mkdir result")?;
        root.validate()?;
        anyhow::ensure!(
            root.directory.read_dir(Path::new(""), 1)?.is_empty(),
            "checkout root acquired preexisting data"
        );
        let plan = &mut self.git[index];
        let repository = crate::git::open_repo_at(repo)?;
        let target = repository
            .find_reference(&format!("refs/heads/{branch}"))
            .ok()
            .and_then(|reference| reference.target());
        anyhow::ensure!(
            target == plan.produced.or(plan.before),
            "original admitted branch changed before bootstrap effects"
        );
        // Check every existing linked HEAD, including a locked or missing worktree.
        for name in repository.worktrees()?.iter() {
            let name = name?.context("non-UTF8 protected worktree name")?;
            let directory = plan
                .common
                .directory
                .child(&Path::new("worktrees").join(name))?;
            let head = directory
                .read_regular(Path::new("HEAD"), 4096)?
                .context("preexisting linked HEAD unavailable")?;
            anyhow::ensure!(
                head != format!("ref: refs/heads/{branch}\n").as_bytes(),
                "branch is already checked out by a protected worktree"
            );
        }
        let source_head = repository.head().ok();
        anyhow::ensure!(
            source_head.as_ref().map(|head| head.name()).transpose()?
                != Some(format!("refs/heads/{branch}").as_str())
                || repository.is_bare(),
            "branch is already checked out by the original source worktree"
        );
        plan.source_git.validate()?;
        if let Some(file) = &plan.config_source {
            file.validate(&plan.source_git.directory)?;
        }
        if let Some(file) = &plan.sparse_source {
            file.validate(&plan.source_git.directory)?;
        }
        if let Some(parent) = &plan.worktrees_parent {
            parent.validate()?;
        } else {
            let directory = plan
                .common
                .directory
                .create_fresh_child(Path::new("worktrees"))?;
            let birth = directory.birth_identity()?;
            self.directories.push(DirectoryPin {
                mode: directory.permissions_mode()?,
                directory: directory.child(Path::new(""))?,
                birth,
                parent_undo_leaf: None,
            });
            plan.worktrees_parent = Some(DirectoryPin {
                mode: directory.permissions_mode()?,
                directory,
                birth,
                parent_undo_leaf: None,
            });
        }
        anyhow::ensure!(
            !plan.preexisting_worktrees.contains(&plan.admin_name),
            "original admin name was already protected before effects"
        );
        let parent = plan
            .worktrees_parent
            .as_ref()
            .context("original worktrees parent missing")?;
        let directory = parent
            .directory
            .create_fresh_child(Path::new(&plan.admin_name))?;
        let birth = directory.birth_identity()?;
        let pin = DirectoryPin {
            mode: directory.permissions_mode()?,
            directory,
            birth,
            parent_undo_leaf: None,
        };
        // Install actual mkdir custody before the first following file effect.
        plan.admin = Some(GitAdmin {
            seed: Tree::capture(&pin.directory)?,
            pin,
            result: None,
            consuming: false,
        });
        plan.admin_uncertainty =
            Some("original Git bootstrap has not acknowledged all layout file effects".into());
        let admin = plan.admin.as_mut().unwrap();
        let admin_path = admin.pin.directory.path();
        let relative = crate::git::GitWorktree::diff_paths(admin_path, path)
            .context("Git layout roots have no relative path")?;
        use std::os::unix::ffi::OsStrExt;
        let mut pointer = b"gitdir: ".to_vec();
        pointer.extend_from_slice(relative.as_os_str().as_bytes());
        pointer.push(b'\n');
        Self::allocate_git_file(&root.directory, Path::new(".git"), &pointer)?;
        Self::allocate_git_file(
            &admin.pin.directory,
            Path::new("HEAD"),
            format!("ref: refs/heads/{branch}\n").as_bytes(),
        )?;
        Self::allocate_git_file(&admin.pin.directory, Path::new("commondir"), b"../..\n")?;
        let mut backlink = path.join(".git").as_os_str().as_bytes().to_vec();
        backlink.push(b'\n');
        Self::allocate_git_file(&admin.pin.directory, Path::new("gitdir"), &backlink)?;
        Self::allocate_git_file(
            &admin.pin.directory,
            Path::new("locked"),
            format!("{lock_reason}\n").as_bytes(),
        )?;
        let refs = admin.pin.directory.create_fresh_child(Path::new("refs"))?;
        for name in ["bisect", "worktree", "rewritten"] {
            refs.create_fresh_child(Path::new(name))?;
        }
        if let Some(source) = &plan.sparse_source {
            let info = admin.pin.directory.create_fresh_child(Path::new("info"))?;
            Self::allocate_git_file(&info, Path::new("sparse-checkout"), &source.bytes)?;
        }
        if let Some(source) = &plan.config_source {
            Self::allocate_git_file(
                &admin.pin.directory,
                Path::new("config.worktree"),
                &source.bytes,
            )?;
            let mut config = git2::Config::open(&admin_path.join("config.worktree"))?;
            // The same filtering as Git's copy_filtered_worktree_config.
            if config.get_bool("core.bare").unwrap_or(false) {
                config.remove("core.bare")?;
            }
            if config.get_string("core.worktree").is_ok() {
                config.remove("core.worktree")?;
            }
        }
        admin.seed = Tree::capture(&admin.pin.directory)?;
        resource.tree = Some(Tree::capture(&root.directory)?);
        resource.tree_uncertainty = None;
        plan.bootstrap_complete = true;
        plan.admin_uncertainty = None;
        Ok(())
    }

    pub(crate) fn begin_worktree_effect(
        &self,
        intent: &super::builder::CreationIntent,
        repo: &Path,
        branch: &str,
        path: &Path,
    ) -> Result<OwnedWorktreeLayout> {
        let plan = self
            .git
            .iter()
            .find(|plan| plan.repo == repo && plan.branch == branch && plan.path == path)
            .context("checkout is outside its original frozen bootstrap")?;
        anyhow::ensure!(
            plan.bootstrap_complete && !plan.checkout_started,
            "original bootstrap is incomplete or already consumed"
        );
        plan.common.validate()?;
        let admin = plan
            .admin
            .as_ref()
            .context("original admin producer result missing")?;
        admin.pin.validate()?;
        admin.seed.validate(&admin.pin.directory)?;
        let resource = self
            .paths
            .iter()
            .find(|resource| resource.path == path)
            .context("original checkout path missing")?;
        resource.parent.validate()?;
        let root = resource
            .produced
            .as_ref()
            .context("original root producer result missing")?;
        root.validate()?;
        resource
            .tree
            .as_ref()
            .context("original root seed missing")?
            .validate(&root.directory)?;
        let expected = plan
            .produced
            .or(plan.before)
            .context("actual branch producer OID missing")?;
        anyhow::ensure!(
            crate::git::open_repo_at(repo)?
                .find_reference(&format!("refs/heads/{branch}"))?
                .target()
                == Some(expected),
            "original branch moved before checkout admission"
        );
        let token = |pin: &DirectoryPin| {
            super::builder::AnchoredDir::from_original(intent, &pin.directory, pin.birth)
        };
        Ok(OwnedWorktreeLayout {
            root: token(root)?,
            admin: token(&admin.pin)?,
            common: token(&plan.common)?,
            oid: expected,
        })
    }

    pub(crate) fn begin_checkout_command(
        &mut self,
        repo: &Path,
        branch: &str,
        path: &Path,
    ) -> Result<()> {
        let plan = self
            .git
            .iter_mut()
            .find(|plan| plan.repo == repo && plan.branch == branch && plan.path == path)
            .context("checkout command has no original producer")?;
        anyhow::ensure!(
            plan.bootstrap_complete && !plan.checkout_started,
            "original checkout command admission changed"
        );
        plan.common.validate()?;
        plan.admin
            .as_ref()
            .context("original admin missing")?
            .pin
            .validate()?;
        plan.checkout_started = true; // Before spawn, not inferred later from journals.
        Ok(())
    }

    pub(crate) fn original_worktree_lock(&self, path: &Path) -> Result<()> {
        let plan = self
            .git
            .iter()
            .find(|plan| plan.path == path)
            .context("lock is outside original plan")?;
        let admin = plan
            .admin
            .as_ref()
            .context("lock has no original admin producer")?;
        admin.pin.validate()?;
        admin.seed.validate_seed(&admin.pin.directory)
    }
    pub(crate) fn retain_submodule_domain(&mut self, path: &Path) -> Result<()> {
        let plan = self
            .git
            .iter_mut()
            .find(|plan| plan.path == path)
            .context("submodule command has no original plan")?;
        plan.submodule_uncertainty = Some(format!("submodule update may mutate daemon/remote and shared Git module admin domains under {}; these were not issued by the original checkout/admin allocator", plan.common.directory.path().join("modules").display()));
        Ok(())
    }
    pub(crate) fn prepare_tracking(&mut self, repo: &Path, branch: &str) -> Result<()> {
        let plan = self
            .git
            .iter_mut()
            .find(|plan| plan.repo == repo && plan.branch == branch)
            .context("tracking producer outside original plan")?;
        let config = git2::Config::open(&plan.common.directory.path().join("config"))?;
        anyhow::ensure!(
            tracking_values(&config, &format!("branch.{branch}.remote"))? == plan.before_tracking.0
                && tracking_values(&config, &format!("branch.{branch}.merge"))?
                    == plan.before_tracking.1,
            "original tracking keys changed before producer effect"
        );
        plan.tracking_uncertainty =
            Some("tracking writer has not issued its complete actual effect result".into());
        Ok(())
    }
    pub(crate) fn acknowledge_tracking(
        &mut self,
        repo: &Path,
        branch: &str,
        remote: &str,
        merge: &str,
    ) -> Result<()> {
        let plan = self
            .git
            .iter_mut()
            .find(|plan| plan.repo == repo && plan.branch == branch)
            .context("tracking branch has no original producer")?;
        anyhow::ensure!(
            plan.before.is_none() && plan.produced.is_some() && plan.tracking.is_none(),
            "tracking is not newly produced by this Create"
        );
        let config = git2::Config::open(&plan.common.directory.path().join("config"))?;
        anyhow::ensure!(
            config.get_string(&format!("branch.{branch}.remote"))? == remote
                && config.get_string(&format!("branch.{branch}.merge"))? == merge,
            "actual tracking producer result differs from its intended keys"
        );
        plan.tracking = Some(TrackingUndo {
            remote: remote.to_owned(),
            merge: merge.to_owned(),
            removed: [false; 2],
            restored: [0; 2],
        });
        plan.tracking_uncertainty = None;
        Ok(())
    }
    pub(crate) fn acknowledge_created_branch(
        &mut self,
        repo: &Path,
        branch: &str,
        produced: git2::Oid,
    ) -> Result<()> {
        let plan = self
            .git
            .iter_mut()
            .find(|plan| plan.repo == repo && plan.branch == branch)
            .context("branch producer has no original pre-effect proof")?;
        plan.common.validate()?;
        anyhow::ensure!(
            plan.before.is_none() && plan.produced.is_none(),
            "branch was not newly produced"
        );
        plan.produced = Some(produced);
        let actual = crate::git::open_repo_at(repo)?
            .find_reference(&format!("refs/heads/{branch}"))?
            .target();
        anyhow::ensure!(
            actual == Some(produced),
            "actual branch changed before its effect acknowledgement"
        );
        Ok(())
    }
    pub(crate) fn acknowledge_worktree(
        &mut self,
        repo: &Path,
        branch: &str,
        path: &Path,
        complete: bool,
    ) -> Result<()> {
        let plan = self
            .git
            .iter_mut()
            .find(|plan| plan.repo == repo && plan.branch == branch && plan.path == path)
            .context("worktree has no original pre-effect proof")?;
        plan.common.validate()?;
        let resource = self
            .paths
            .iter_mut()
            .find(|resource| resource.path == path)
            .context("original checkout plan missing")?;
        resource.parent.validate()?;
        let root = resource
            .produced
            .as_ref()
            .context("original checkout mkdir receipt missing")?;
        root.validate()?;
        let admin = plan
            .admin
            .as_mut()
            .context("original admin mkdir receipt missing")?;
        admin.pin.validate()?;
        anyhow::ensure!(
            !admin.consuming,
            "original admin is already being withdrawn"
        );
        admin.seed.validate_seed(&admin.pin.directory)?;
        let first_result = plan.add_outcome.is_none();
        let succeeded = plan.add_outcome.unwrap_or(true) && complete;
        plan.add_outcome = Some(succeeded);
        plan.checkout_acknowledged = succeeded;
        if !succeeded {
            plan.admin_uncertainty =
                Some("original index/checkout command did not acknowledge complete effects".into());
            resource.tree_uncertainty =
                Some("partial checkout index/working-file output remains protected".into());
            return Ok(());
        }
        let repository = git2::Repository::open(path)?;
        anyhow::ensure!(
            DirectoryPin::capture(&common_directory(&repository)?)?.birth == plan.common.birth,
            "checkout common-directory relationship changed"
        );
        anyhow::ensure!(
            repository.path().canonicalize()? == admin.pin.directory.path(),
            "checkout points at a different admin resource"
        );
        let expected = plan
            .produced
            .or(plan.before)
            .context("original branch OID missing")?;
        anyhow::ensure!(
            repository.head()?.name()? == format!("refs/heads/{branch}"),
            "original checkout branch changed"
        );
        if repository.head()?.target() != Some(expected) {
            plan.clean = false;
            plan.admin_uncertainty =
                Some("original checkout tip changed after its producer ACK".into());
            return Ok(());
        }
        // Only names issued by our allocator or specified native index/log
        // outputs are admissible. Arbitrary admin additions remain protected.
        for name in admin.pin.directory.read_dir(Path::new(""), usize::MAX)? {
            let name = name
                .to_str()
                .context("non-UTF8 admin output remains protected")?;
            anyhow::ensure!(
                matches!(
                    name,
                    "HEAD"
                        | "commondir"
                        | "gitdir"
                        | "locked"
                        | "refs"
                        | "info"
                        | "config.worktree"
                        | "index"
                ) || (name.starts_with("sharedindex.")
                    && name.len() == 52
                    && name[12..].bytes().all(|b| b.is_ascii_hexdigit())),
                "unadmitted admin output remains protected: {name}"
            );
        }
        plan.clean &= clean_checkout(&repository)?;
        if !plan.clean {
            resource.tree_uncertainty =
                Some("checkout contains dirty, untracked or ignored data".into());
            return Ok(());
        }
        if first_result {
            resource
                .tree
                .as_ref()
                .context("original root allocation seed missing")?
                .validate_seed(&root.directory)?;
            match (
                Tree::capture(&root.directory),
                Tree::capture(&admin.pin.directory),
            ) {
                (Ok(tree), Ok(admin_tree)) => {
                    resource.tree = Some(tree);
                    resource.tree_uncertainty = None;
                    admin.result = Some(admin_tree);
                    plan.admin_uncertainty = None;
                }
                (body, admin_body) => {
                    resource.tree = None;
                    resource.tree_uncertainty = body.err().map(|error| format!("{error:#}"));
                    plan.admin_uncertainty = Some(
                        admin_body
                            .err()
                            .map(|error| format!("{error:#}"))
                            .unwrap_or_else(|| {
                                "checkout output cannot be completely certified".into()
                            }),
                    );
                    return Ok(());
                }
            }
        } else {
            // Never rebase the original byte proof over post-checkout/on_create
            // user output, even when index flags or filters hide it from status.
            if resource.tree.is_none() || admin.result.is_none() {
                return Ok(());
            }
            let unchanged = resource
                .tree
                .as_ref()
                .context("original body ACK missing")?
                .validate(&root.directory)
                .and_then(|()| {
                    admin
                        .result
                        .as_ref()
                        .context("original admin ACK missing")?
                        .validate(&admin.pin.directory)
                });
            if let Err(error) = unchanged {
                plan.clean = false;
                resource.tree_uncertainty = Some(format!(
                    "original post-checkout body/admin changed: {error:#}"
                ));
            }
        }
        root.validate()?;
        admin.pin.validate()?;
        Ok(())
    }
    pub(crate) fn retain_container(&mut self, instance: &Instance) -> Result<()> {
        let sandbox = instance
            .sandbox_info
            .as_ref()
            .context("container intent lacks sandbox plan")?;
        let plan = (
            sandbox.container_name.clone(),
            instance.container_workdir(),
            sandbox.image.clone(),
        );
        anyhow::ensure!(
            self.container
                .as_ref()
                .is_none_or(|original| original == &plan),
            "container plan changed"
        );
        self.container = Some(plan);
        Ok(())
    }
    pub(crate) fn retain_container_goal(&mut self, argv: &[String]) {
        self.container_goals.push(argv.to_vec());
    }
    pub(crate) fn acknowledge_container_result(&mut self, id: &str) {
        // Actual CLI result, deliberately NOT daemon/container birth authority.
        self.container_results.push(id.to_owned());
    }
    fn bind_source_command(
        intent: &super::builder::CreationIntent,
        plan: &GitPlan,
        command: &mut super::runner_journal::OwnedCreateCommand,
    ) -> Result<()> {
        let token = |pin: &DirectoryPin| -> Result<super::builder::AnchoredDir> {
            pin.validate()?;
            super::builder::AnchoredDir::from_original(intent, &pin.directory, pin.birth)
        };
        command.anchored_current_dir(&token(&plan.source)?)?;
        command.anchored_env_path("GIT_DIR", &token(&plan.source_git)?)?;
        command.anchored_env_path("GIT_COMMON_DIR", &token(&plan.common)?)?;
        command.env_remove("GIT_WORK_TREE").env("LC_ALL", "C");
        Ok(())
    }
    fn stage_original(pin: &mut DirectoryPin, parent: &AnchoredDir, undo_leaf: &str) -> Result<()> {
        if pin.directory.path().file_name() == Some(std::ffi::OsStr::new(undo_leaf)) {
            pin.validate()?;
            return Ok(());
        }
        pin.validate()?;
        let source = pin
            .directory
            .path()
            .file_name()
            .context("original resource has no leaf")?;
        anyhow::ensure!(
            parent.move_original_entry(Path::new(source), Path::new(undo_leaf))?,
            "original Undo staging name acquired by another producer"
        );
        pin.directory = pin.directory.relocated(parent.path().join(undo_leaf))?;
        let staged = parent.child(Path::new(undo_leaf))?;
        anyhow::ensure!(
            staged.birth_identity()? == pin.birth && pin.directory.birth_identity()? == pin.birth,
            "staged resource is not the original producer birth; uncertain stage retained"
        );
        parent.sync()?;
        Ok(())
    }
    fn flush_original_sync(&mut self) -> Result<()> {
        while let Some(directory) = self.pending_sync.last() {
            directory.sync()?;
            self.pending_sync.pop();
        }
        Ok(())
    }
    pub(crate) fn undo(&mut self, intent: &super::builder::CreationIntent) -> Result<()> {
        if let Some((name, workdir, _)) = &self.container {
            anyhow::bail!("original daemon/container birth custody unavailable for {name} at {workdir}; {} exact goals and {} actual CLI results retained",
                self.container_goals.len(), self.container_results.len());
        }
        self.flush_original_sync()?;
        // Validate the complete original resource set before the first destructive effect.
        {
            let _fences = intent.original_undo_fences()?;
            for path in &self.paths {
                path.parent.validate()?;
                if path.preexisting {
                    continue;
                }
                if path.removed {
                    continue;
                }
                if let Some(pin) = &path.produced {
                    pin.validate()?;
                    let managed = self.git.iter().any(|plan| plan.path == path.path);
                    if managed {
                        path.tree
                            .as_ref()
                            .with_context(|| {
                                path.tree_uncertainty.clone().unwrap_or_else(|| {
                                    "checkout effect acknowledgement is incomplete".to_owned()
                                })
                            })?
                            .validate(&pin.directory)?;
                    }
                } else {
                    anyhow::ensure!(
                        !path.path.try_exists()?,
                        "unacknowledged creation artifact remains protected"
                    );
                }
            }
            for plan in &self.git {
                plan.common.validate()?;
                if plan.removed {
                    continue;
                }
                anyhow::ensure!(
                    plan.admin.is_none() || plan.bootstrap_complete,
                    "partial original Git layout allocation remains protected"
                );
                let repository = crate::git::open_repo_at(&plan.repo)?;
                let target = repository
                    .find_reference(&format!("refs/heads/{}", plan.branch))
                    .ok()
                    .and_then(|r| r.target());
                anyhow::ensure!(
                    target
                        == if plan.branch_deleted {
                            None
                        } else {
                            plan.produced.or(plan.before)
                        },
                    "branch changed after original production"
                );
                if !plan.layout_removed && plan.bootstrap_complete {
                    if plan.checkout_acknowledged {
                        anyhow::ensure!(
                            plan.clean && plan.admin_uncertainty.is_none(),
                            "dirty/incomplete native checkout remains protected"
                        );
                    } else {
                        // The admitted command failed; only equality with the
                        // ORIGINAL pre-effect root/admin seed can authorize Undo.
                        plan.admin
                            .as_ref()
                            .context("original seed missing")?
                            .seed
                            .validate(&plan.admin.as_ref().unwrap().pin.directory)?;
                    }
                    if let Some(admin) = &plan.admin {
                        admin.pin.validate()?;
                        if !admin.consuming {
                            admin.seed.validate_seed(&admin.pin.directory)?;
                        }
                        if plan.checkout_acknowledged {
                            admin
                                .result
                                .as_ref()
                                .context("original admin effect ACK incomplete")?
                                .validate(&admin.pin.directory)?;
                        } else {
                            admin.seed.validate(&admin.pin.directory)?;
                        }
                    }
                    if !self
                        .paths
                        .iter()
                        .any(|path| path.path == plan.path && path.removed)
                    {
                        let resource = self
                            .paths
                            .iter()
                            .find(|path| path.path == plan.path)
                            .context("original path proof missing")?;
                        let root = resource
                            .produced
                            .as_ref()
                            .context("original checkout birth missing")?;
                        if plan.checkout_acknowledged && !resource.consuming {
                            let checkout = git2::Repository::open(root.directory.path())?;
                            anyhow::ensure!(
                                clean_checkout(&checkout)?,
                                "checkout contains user/hook changes"
                            );
                            anyhow::ensure!(
                                checkout.head()?.target() == target,
                                "checkout moved from original branch tip"
                            );
                        }
                    }
                }
                // Retain the complete protected baseline; undo never prunes shared Git
                // entries, restores somebody else's dirty files, or deletes fetched refs.
                if plan.before.is_none() {
                    anyhow::ensure!(
                        !plan
                            .before_refs
                            .iter()
                            .any(|(name, _)| name == &format!("refs/heads/{}", plan.branch)),
                        "preexisting symbolic branch remains protected"
                    );
                }
                if let Some(tracking) = &plan.tracking {
                    let config = git2::Config::open(&plan.common.directory.path().join("config"))?;
                    for (slot, suffix, actual, original) in [
                        (
                            0,
                            "remote",
                            tracking.remote.as_str(),
                            &plan.before_tracking.0,
                        ),
                        (1, "merge", tracking.merge.as_str(), &plan.before_tracking.1),
                    ] {
                        let expected = if tracking.removed[slot] {
                            original[..tracking.restored[slot]].to_vec()
                        } else {
                            vec![actual.to_owned()]
                        };
                        anyhow::ensure!(
                            tracking_values(&config, &format!("branch.{}.{suffix}", plan.branch))?
                                == expected,
                            "tracking resource changed before original Undo"
                        );
                    }
                }
                anyhow::ensure!(
                    plan.tracking_uncertainty.is_none(),
                    "{}",
                    plan.tracking_uncertainty.as_deref().unwrap_or("")
                );
                anyhow::ensure!(
                    plan.submodule_uncertainty.is_none(),
                    "{}",
                    plan.submodule_uncertainty.as_deref().unwrap_or("")
                );
                plan.source.validate()?;
                for dirty in &plan.protected_dirty {
                    dirty.validate(&plan.source.directory)?;
                }
                for (name, original) in &plan.before_refs {
                    if name.starts_with("refs/heads/")
                        && name != &format!("refs/heads/{}", plan.branch)
                    {
                        anyhow::ensure!(
                            repository
                                .find_reference(name)
                                .ok()
                                .and_then(|reference| reference.target())
                                == *original,
                            "protected source branch changed: {name}"
                        );
                    }
                }
                let current_dirty: Vec<_> = if repository.is_bare() {
                    Vec::new()
                } else {
                    checkout_statuses(&repository)?
                        .iter()
                        .map(|entry| (entry.path_bytes().to_vec(), entry.status()))
                        .collect()
                };
                anyhow::ensure!(
                    current_dirty == plan.before_dirty,
                    "protected source checkout changed during creation"
                );
            }
        }
        for index in 0..self.git.len() {
            if self.git[index].removed {
                continue;
            }
            if !self.git[index].layout_removed {
                let resource = self
                    .paths
                    .iter_mut()
                    .find(|path| path.path == self.git[index].path)
                    .context("original checkout proof missing")?;
                if let Some(root) = resource.produced.as_mut().filter(|_| !resource.removed) {
                    let _fences = intent.original_undo_fences()?;
                    let parent_path = root
                        .directory
                        .path()
                        .parent()
                        .context("original checkout parent missing")?;
                    let parent = if resource.parent.directory.path() == parent_path {
                        resource.parent.directory.child(Path::new(""))?
                    } else {
                        let original = self
                            .directories
                            .iter()
                            .find(|pin| pin.directory.path() == parent_path)
                            .context("checkout parent is not an original admitted producer")?;
                        original.validate()?;
                        original.directory.child(Path::new(""))?
                    };
                    Self::stage_original(root, &parent, &resource.undo_leaf)?;
                    resource.consuming = true;
                    resource
                        .tree
                        .as_mut()
                        .context("original checkout effect proof missing")?
                        .consume(&root.directory)?;
                    parent.remove_empty_child(Path::new(&resource.undo_leaf))?;
                    resource.removed = true; // Actual original rmdir ACK, not absence.
                    self.directories
                        .retain(|pin| pin.directory.path() != resource.path);
                    self.pending_sync.push(parent);
                    self.flush_original_sync()?;
                }
                let plan = &mut self.git[index];
                if let Some(admin) = &mut plan.admin {
                    let _fences = intent.original_undo_fences()?;
                    let parent = plan
                        .worktrees_parent
                        .as_ref()
                        .context("original admin parent missing")?;
                    parent.validate()?;
                    Self::stage_original(&mut admin.pin, &parent.directory, &plan.admin_undo_leaf)?;
                    let proof = if plan.checkout_acknowledged {
                        admin
                            .result
                            .as_mut()
                            .context("original native admin effect proof missing")?
                    } else {
                        &mut admin.seed
                    };
                    admin.consuming = true;
                    proof.consume(&admin.pin.directory)?;
                    parent
                        .directory
                        .remove_empty_child(Path::new(&plan.admin_undo_leaf))?;
                    plan.admin = None; // Commit actual rmdir ACK before fallible sync.
                    self.pending_sync
                        .push(parent.directory.child(Path::new(""))?);
                }
                plan.layout_removed = true;
            }
            self.flush_original_sync()?;
            let plan = &mut self.git[index];
            if plan.before.is_none() {
                if let Some(oid) = plan.produced {
                    if let Some(mut tracking) = plan.tracking.take() {
                        let result = (|| -> Result<()> {
                            for slot in 0..2 {
                                let (suffix, actual, original) = if slot == 0 {
                                    ("remote", tracking.remote.as_str(), &plan.before_tracking.0)
                                } else {
                                    ("merge", tracking.merge.as_str(), &plan.before_tracking.1)
                                };
                                let key = format!("branch.{}.{suffix}", plan.branch);
                                if !tracking.removed[slot] {
                                    let config = git2::Config::open(
                                        &plan.common.directory.path().join("config"),
                                    )?;
                                    anyhow::ensure!(
                                        tracking_values(&config, &key)? == [actual.to_owned()],
                                        "original tracking value changed"
                                    );
                                    let mut remove = intent.owned_undo_command("git")?;
                                    Self::bind_source_command(intent, plan, &mut remove)?;
                                    remove.args([
                                        "config",
                                        "--local",
                                        "--fixed-value",
                                        "--unset-all",
                                        &key,
                                        actual,
                                    ]);
                                    let output = intent.run_owned_output(&mut remove, None)?;
                                    anyhow::ensure!(
                                        output.status.success(),
                                        "original tracking-value CAS Undo failed"
                                    );
                                    tracking.removed[slot] = true;
                                    intent.retire_owned_commands()?;
                                }
                                let config = git2::Config::open(
                                    &plan.common.directory.path().join("config"),
                                )?;
                                anyhow::ensure!(
                                    tracking_values(&config, &key)?
                                        == original[..tracking.restored[slot]],
                                    "tracking restore acquired third-party data"
                                );
                                while tracking.restored[slot] < original.len() {
                                    let value = &original[tracking.restored[slot]];
                                    let mut restore = intent.owned_undo_command("git")?;
                                    Self::bind_source_command(intent, plan, &mut restore)?;
                                    restore.args(["config", "--local", "--add", &key, value]);
                                    let output = intent.run_owned_output(&mut restore, None)?;
                                    anyhow::ensure!(
                                        output.status.success(),
                                        "original tracking-value restoration failed"
                                    );
                                    tracking.restored[slot] += 1;
                                    intent.retire_owned_commands()?;
                                }
                            }
                            Ok(())
                        })();
                        if let Err(error) = result {
                            plan.tracking = Some(tracking);
                            return Err(error);
                        }
                    }
                    if !plan.branch_deleted {
                        let mut remove = intent.owned_undo_command("git")?;
                        Self::bind_source_command(intent, plan, &mut remove)?;
                        remove.args([
                            "update-ref",
                            "-d",
                            &format!("refs/heads/{}", plan.branch),
                            &oid.to_string(),
                        ]);
                        let output = intent.run_owned_output(&mut remove, None)?;
                        anyhow::ensure!(output.status.success(), "original branch CAS undo failed");
                        plan.branch_deleted = true;
                    }
                    #[cfg(test)]
                    if FAIL_BRANCH_RETIRE_ONCE.replace(false) {
                        anyhow::bail!("injected original branch retirement failure");
                    }
                    intent.retire_owned_commands()?;
                }
            }
            plan.removed = true;
        }
        // Only directories whose successful mkdir we acknowledged are consumed.
        let _fences = intent.original_undo_fences()?;
        while !self.directories.is_empty() {
            let (pin, ancestors) = self.directories.split_last_mut().unwrap();
            pin.validate()?;
            anyhow::ensure!(
                pin.directory.read_dir(Path::new(""), 1)?.is_empty(),
                "created directory contains retained data"
            );
            let parent_path = pin
                .directory
                .path()
                .parent()
                .context("created directory parent missing")?;
            let parent = if let Some(original) = ancestors
                .iter()
                .find(|original| original.directory.path() == parent_path)
            {
                original.validate()?;
                original.directory.child(Path::new(""))?
            } else if let Some(original) = self
                .paths
                .iter()
                .map(|path| &path.parent)
                .find(|original| original.directory.path() == parent_path)
            {
                original.validate()?;
                original.directory.child(Path::new(""))?
            } else {
                let original = self
                    .git
                    .iter()
                    .map(|plan| &plan.common)
                    .find(|original| original.directory.path() == parent_path)
                    .context("mkdir parent has no retained original physical capability")?;
                original.validate()?;
                original.directory.child(Path::new(""))?
            };
            let leaf = pin
                .parent_undo_leaf
                .get_or_insert_with(|| format!(".aoe-parent-undo-{}", uuid::Uuid::new_v4()))
                .clone();
            Self::stage_original(pin, &parent, &leaf)?;
            anyhow::ensure!(
                pin.directory.read_dir(Path::new(""), 1)?.is_empty(),
                "staged original parent acquired data"
            );
            parent.remove_empty_child(Path::new(&leaf))?;
            self.directories.pop(); // Actual rmdir ACK before fallible fsync.
            self.pending_sync.push(parent);
            self.flush_original_sync()?;
        }
        self.flush_original_sync()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn producer_mkdir_ack_keeps_original_birth_and_rejects_replaced_parent() {
        let home = tempfile::tempdir().unwrap();
        let parent = home.path().join("parent");
        std::fs::create_dir(&parent).unwrap();
        let path = parent.join("scratch");
        let mut instance = Instance::new("scratch", path.to_str().unwrap());
        instance.scratch = true;
        let mut undo = CreationUndo::freeze(&instance, std::slice::from_ref(&path)).unwrap();
        assert!(!path.exists());
        undo.provision(&path).unwrap();
        let produced = undo.paths[0].produced.as_ref().unwrap();
        assert_eq!(produced.birth, produced.directory.birth_identity().unwrap());
        let moved = home.path().join("old-parent");
        std::fs::rename(&parent, &moved).unwrap();
        std::fs::create_dir(&parent).unwrap();
        assert!(produced.validate().is_err());
        assert!(undo.paths[0].parent.validate().is_err());
        assert!(moved.join("scratch").is_dir());
    }
    #[test]
    fn snapshot_preserves_modified_or_replaced_same_content_files() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("file");
        std::fs::write(&path, b"original").unwrap();
        let anchor = AnchoredDir::open(home.path()).unwrap();
        let tree = Tree::capture(&anchor).unwrap();
        std::fs::write(&path, b"changed").unwrap();
        assert!(tree.validate(&anchor).is_err());
        std::fs::write(&path, b"original").unwrap();
        tree.validate(&anchor).unwrap();
        std::fs::rename(&path, home.path().join("old-file")).unwrap();
        std::fs::write(&path, b"original").unwrap();
        assert!(tree.validate(&anchor).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"original");
    }
    #[test]
    fn preexisting_resources_are_not_acknowledged_as_produced() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("scratch");
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("user-data"), b"keep").unwrap();
        let mut instance = Instance::new("scratch", path.to_str().unwrap());
        instance.scratch = true;
        let mut undo = CreationUndo::freeze(&instance, std::slice::from_ref(&path)).unwrap();
        assert!(undo.provision(&path).is_err());
        assert!(undo.paths[0].produced.is_none());
        assert_eq!(std::fs::read(path.join("user-data")).unwrap(), b"keep");
    }

    #[test]
    fn streamed_commitment_detects_same_length_change_past_the_first_buffer() {
        use std::io::{Seek, SeekFrom, Write};
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("large-file");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(16 * 1024 * 1024).unwrap();
        drop(file);
        let anchor = AnchoredDir::open(home.path()).unwrap();
        let tree = Tree::capture(&anchor).unwrap();
        match &tree.entries[0].1 {
            Entry::File { commitment, .. } => assert_eq!(commitment.length, 16 * 1024 * 1024),
            Entry::Directory(_) => panic!("fixture is a regular file"),
        }
        tree.validate(&anchor).unwrap();
        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(12 * 1024 * 1024 + 37)).unwrap();
        file.write_all(b"changed").unwrap();
        drop(file);
        assert!(tree.validate(&anchor).is_err());
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16 * 1024 * 1024);
    }

    #[test]
    fn streamed_commitment_protects_permissions_even_when_bytes_are_unchanged() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("mode-file");
        std::fs::write(&path, b"same bytes").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let anchor = AnchoredDir::open(home.path()).unwrap();
        let tree = Tree::capture(&anchor).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(tree.validate(&anchor).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"same bytes");
    }

    #[test]
    fn mkdir_ack_cannot_promote_a_third_party_replacement_path() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("original");
        let mut instance = Instance::new("scratch", path.to_str().unwrap());
        instance.scratch = true;
        let mut undo = CreationUndo::freeze(&instance, std::slice::from_ref(&path)).unwrap();
        undo.provision(&path).unwrap();
        let birth = undo.paths[0].produced.as_ref().unwrap().birth;
        std::fs::rename(&path, home.path().join("retained-original")).unwrap();
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("third-party-data"), b"keep").unwrap();
        let original = undo
            .directories
            .iter()
            .find(|pin| pin.directory.path() == path)
            .unwrap();
        assert!(undo.paths[0].acknowledge_mkdir(original).is_err());
        assert_eq!(undo.paths[0].produced.as_ref().unwrap().birth, birth);
        assert_eq!(
            std::fs::read(path.join("third-party-data")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn original_tree_directory_permission_change_is_dirty() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let anchor = AnchoredDir::open(home.path()).unwrap();
        let tree = Tree::capture(&anchor).unwrap();
        std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(tree.validate(&anchor).is_err());
    }

    #[test]
    fn original_seed_never_certifies_new_admin_child_data() {
        let home = tempfile::tempdir().unwrap();
        let anchor = AnchoredDir::open(home.path()).unwrap();
        let refs = anchor.create_fresh_child(Path::new("refs")).unwrap();
        let seed = Tree::capture(&anchor).unwrap();
        std::fs::write(refs.path().join("third-party-data"), b"keep").unwrap();
        assert!(seed.validate_seed(&anchor).is_err());
        assert_eq!(
            std::fs::read(refs.path().join("third-party-data")).unwrap(),
            b"keep"
        );
    }

    #[test]
    fn original_tree_consume_records_real_unlink_acks() {
        let home = tempfile::tempdir().unwrap();
        let anchor = AnchoredDir::open(home.path()).unwrap();
        std::fs::write(home.path().join("owned"), b"bytes").unwrap();
        let mut tree = Tree::capture(&anchor).unwrap();
        tree.consume(&anchor).unwrap();
        assert!(tree.entries.is_empty());
        assert!(anchor.read_dir(Path::new(""), 1).unwrap().is_empty());
        tree.validate(&anchor).unwrap();
    }
}
