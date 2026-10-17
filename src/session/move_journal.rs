//! Durable logical resource intents for short namespace transactions and plain Git.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::environment::shell_escape_script_word;
use super::storage::{atomic_write_verified, sync_parent_directory};

/// Historical journal layouts are converted by the migration registry.
pub(crate) const MOVE_JOURNAL_VERSION: u32 = 4;

const JOURNAL_DIR_NAME: &str = ".move-journal";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ProfileAuthority {
    pub(crate) profile: String,
    pub(crate) sessions_path: PathBuf,
}

impl ProfileAuthority {
    pub(crate) fn capture(storage: &super::Storage) -> Result<Self> {
        Ok(Self {
            profile: storage.profile().to_owned(),
            sessions_path: super::deletion::resolve_claim_path(storage.sessions_path())
                .context("profile sessions path cannot be resolved")?,
        })
    }

    pub(crate) fn matches(&self, storage: &super::Storage) -> Result<bool> {
        Ok(
            super::deletion::resolve_claim_path(storage.sessions_path()).as_ref()
                == Some(&self.sessions_path),
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ResourceKey {
    Path {
        path: PathBuf,
        recursive: bool,
    },
    Branch {
        /// Canonical Git common directory, not the checkout directory.
        repository: PathBuf,
        reference: String,
    },
}

impl ResourceKey {
    pub(crate) fn path(path: &Path, recursive: bool) -> Result<Self> {
        Ok(Self::Path {
            path: super::deletion::resolve_claim_path(path)
                .with_context(|| format!("cannot resolve resource path {}", path.display()))?,
            recursive,
        })
    }

    pub(crate) fn branch(repository: &Path, branch: &str) -> Result<Self> {
        let repo = crate::git::open_repo_at(repository)?;
        Ok(Self::Branch {
            repository: fs::canonicalize(repo.commondir())?,
            reference: format!("refs/heads/{branch}"),
        })
    }

    pub(crate) fn overlaps(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Path {
                    path: left,
                    recursive: left_recursive,
                },
                Self::Path {
                    path: right,
                    recursive: right_recursive,
                },
            ) => {
                left == right
                    || (*left_recursive && right.starts_with(left))
                    || (*right_recursive && left.starts_with(right))
            }
            (
                Self::Branch {
                    repository: left_repo,
                    reference: left_ref,
                },
                Self::Branch {
                    repository: right_repo,
                    reference: right_ref,
                },
            ) => left_repo == right_repo && left_ref == right_ref,
            _ => false,
        }
    }

    /// Directory effects also cover Git metadata stored inside that directory.
    pub(crate) fn intent_overlaps(&self, other: &Self) -> bool {
        self.overlaps(other)
            || match (self, other) {
                (
                    Self::Path {
                        path,
                        recursive: true,
                    },
                    Self::Branch { repository, .. },
                )
                | (
                    Self::Branch { repository, .. },
                    Self::Path {
                        path,
                        recursive: true,
                    },
                ) => repository.starts_with(path),
                _ => false,
            }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct IntentOwner {
    pub(crate) profile: ProfileAuthority,
    pub(crate) id: String,
    pub(crate) created_at: chrono::DateTime<chrono::Utc>,
    pub(crate) generation: u64,
}

impl IntentOwner {
    pub(crate) fn matches(&self, profile: &ProfileAuthority, row: &super::Instance) -> bool {
        self.profile.sessions_path == profile.sessions_path
            && self.id == row.id
            && self.created_at == row.created_at
            && self.generation == row.lifecycle_generation
    }
}

#[derive(Debug, PartialEq)]
struct WorktreeClaims {
    branch: String,
    repository: String,
    managed: bool,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl WorktreeClaims {
    fn matches(&self, info: &super::WorktreeInfo) -> bool {
        self.branch == info.branch
            && self.repository == info.main_repo_path
            && self.managed == info.managed_by_aoe
            && self.created_at == info.created_at
    }
}

#[derive(Debug, PartialEq)]
struct WorkspaceRepoClaims {
    source: String,
    repository: String,
    path: String,
    branch: String,
    managed: bool,
    branch_preexisting: bool,
}

impl WorkspaceRepoClaims {
    fn matches(&self, repo: &super::WorkspaceRepo) -> bool {
        self.source == repo.source_path
            && self.repository == repo.main_repo_path
            && self.path == repo.worktree_path
            && self.branch == repo.branch
            && self.managed == repo.managed_by_aoe
            && self.branch_preexisting == repo.branch_preexisting
    }
}

#[derive(Debug, PartialEq)]
struct WorkspaceClaims {
    directory: String,
    branch: String,
    repos: Vec<WorkspaceRepoClaims>,
    cleanup: bool,
}

impl WorkspaceClaims {
    fn matches(&self, workspace: &super::WorkspaceInfo) -> bool {
        self.directory == workspace.workspace_dir
            && self.branch == workspace.branch
            && self.cleanup == workspace.cleanup_on_delete
            && self.repos.len() == workspace.repos.len()
            && self
                .repos
                .iter()
                .zip(&workspace.repos)
                .all(|(claim, repo)| claim.matches(repo))
    }
}

/// Snapshot only fields a generic persistence closure could use to adopt resources.
#[derive(Debug, PartialEq)]
struct RowClaims {
    id: String,
    created_at: chrono::DateTime<chrono::Utc>,
    generation: u64,
    scratch: bool,
    reservation: Option<(super::LifecycleOperation, u64, Option<String>)>,
    project: String,
    pre_trash: Option<String>,
    trashed_at: Option<chrono::DateTime<chrono::Utc>>,
    trash_id: Option<String>,
    worktree: Option<WorktreeClaims>,
    workspace: Option<WorkspaceClaims>,
}

impl RowClaims {
    fn capture(row: &super::Instance) -> Self {
        Self {
            id: row.id.clone(),
            created_at: row.created_at,
            generation: row.lifecycle_generation,
            scratch: row.scratch,
            reservation: row.lifecycle_reservation.as_ref().map(|reservation| {
                (
                    reservation.op,
                    reservation.generation,
                    reservation.resource_intent.clone(),
                )
            }),
            project: row.project_path.clone(),
            pre_trash: row.pre_trash_project_path.clone(),
            trashed_at: row.trashed_at,
            trash_id: row.trash_lifecycle_id.clone(),
            worktree: row.worktree_info.as_ref().map(|info| WorktreeClaims {
                branch: info.branch.clone(),
                repository: info.main_repo_path.clone(),
                managed: info.managed_by_aoe,
                created_at: info.created_at,
            }),
            workspace: row
                .workspace_info
                .as_ref()
                .map(|workspace| WorkspaceClaims {
                    directory: workspace.workspace_dir.clone(),
                    branch: workspace.branch.clone(),
                    cleanup: workspace.cleanup_on_delete,
                    repos: workspace
                        .repos
                        .iter()
                        .map(|repo| WorkspaceRepoClaims {
                            source: repo.source_path.clone(),
                            repository: repo.main_repo_path.clone(),
                            path: repo.worktree_path.clone(),
                            branch: repo.branch.clone(),
                            managed: repo.managed_by_aoe,
                            branch_preexisting: repo.branch_preexisting,
                        })
                        .collect(),
                }),
        }
    }

    fn namespace_matches(&self, row: &super::Instance) -> bool {
        self.scratch == row.scratch
            && self.project == row.project_path
            && self.pre_trash == row.pre_trash_project_path
            && self.trashed_at == row.trashed_at
            && self.trash_id == row.trash_lifecycle_id
            && match (&self.worktree, &row.worktree_info) {
                (Some(claim), Some(info)) => claim.matches(info),
                (None, None) => true,
                _ => false,
            }
            && match (&self.workspace, &row.workspace_info) {
                (Some(claim), Some(workspace)) => claim.matches(workspace),
                (None, None) => true,
                _ => false,
            }
    }

    fn matches(&self, row: &super::Instance) -> bool {
        self.id == row.id
            && self.created_at == row.created_at
            && self.generation == row.lifecycle_generation
            && match (&self.reservation, &row.lifecycle_reservation) {
                (Some((op, generation, intent)), Some(reservation)) => {
                    *op == reservation.op
                        && *generation == reservation.generation
                        && *intent == reservation.resource_intent
                }
                (None, None) => true,
                _ => false,
            }
            && self.namespace_matches(row)
    }

    fn resource_intent(&self) -> Option<&str> {
        self.reservation
            .as_ref()
            .and_then(|(_, _, intent)| intent.as_deref())
    }

    fn branch_claims_match(&self, row: &super::Instance) -> bool {
        let worktree_matches = match (&self.worktree, &row.worktree_info) {
            (Some(claim), Some(info)) => claim.matches(info),
            (None, None) => true,
            _ => false,
        };
        worktree_matches
            && match (&self.workspace, &row.workspace_info) {
                (Some(claim), Some(workspace)) => {
                    claim.branch == workspace.branch
                        && claim.repos.len() == workspace.repos.len()
                        && claim
                            .repos
                            .iter()
                            .zip(&workspace.repos)
                            .all(|(claim, repo)| {
                                claim.repository == repo.main_repo_path
                                    && claim.branch == repo.branch
                                    && claim.managed == repo.managed_by_aoe
                                    && claim.branch_preexisting == repo.branch_preexisting
                            })
                }
                (None, None) => true,
                _ => false,
            }
    }

    fn keys(&self, include_branches: bool) -> Result<Vec<ResourceKey>> {
        let mut keys = Vec::new();
        keys.push(ResourceKey::path(
            Path::new(&self.project),
            self.worktree.is_some() || self.scratch,
        )?);
        if let Some(path) = &self.pre_trash {
            keys.push(ResourceKey::path(Path::new(path), self.worktree.is_some())?);
        }
        if let Some(worktree) = &self.worktree {
            keys.push(ResourceKey::path(Path::new(&worktree.repository), false)?);
            if include_branches {
                keys.push(ResourceKey::branch(
                    Path::new(&worktree.repository),
                    &worktree.branch,
                )?);
            }
        }
        if let Some(workspace) = &self.workspace {
            keys.push(ResourceKey::path(Path::new(&workspace.directory), true)?);
            for repo in &workspace.repos {
                keys.push(ResourceKey::path(Path::new(&repo.path), true)?);
                keys.push(ResourceKey::path(Path::new(&repo.repository), false)?);
                if include_branches {
                    keys.push(ResourceKey::branch(
                        Path::new(&repo.repository),
                        &repo.branch,
                    )?);
                }
            }
        }
        Ok(keys)
    }
}

pub(crate) fn row_resource_keys(row: &super::Instance) -> Result<Vec<ResourceKey>> {
    row_keys(row, true)
}

pub(crate) fn row_path_keys(row: &super::Instance) -> Result<Vec<ResourceKey>> {
    row_keys(row, false)
}

fn row_keys(row: &super::Instance, include_branches: bool) -> Result<Vec<ResourceKey>> {
    let mut keys = Vec::new();
    keys.push(ResourceKey::path(
        Path::new(&row.project_path),
        row.worktree_info.is_some() || row.scratch,
    )?);
    if let Some(path) = &row.pre_trash_project_path {
        keys.push(ResourceKey::path(
            Path::new(path),
            row.worktree_info.is_some(),
        )?);
    }
    if let Some(worktree) = &row.worktree_info {
        keys.push(ResourceKey::path(
            Path::new(&worktree.main_repo_path),
            false,
        )?);
        if include_branches {
            keys.push(ResourceKey::branch(
                Path::new(&worktree.main_repo_path),
                &worktree.branch,
            )?);
        }
    }
    if let Some(workspace) = &row.workspace_info {
        keys.push(ResourceKey::path(
            Path::new(&workspace.workspace_dir),
            true,
        )?);
        for repo in &workspace.repos {
            keys.push(ResourceKey::path(Path::new(&repo.worktree_path), true)?);
            keys.push(ResourceKey::path(Path::new(&repo.main_repo_path), false)?);
            if include_branches {
                keys.push(ResourceKey::branch(
                    Path::new(&repo.main_repo_path),
                    &repo.branch,
                )?);
            }
        }
    }
    Ok(keys)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RowDelta {
    pub(crate) before: super::Instance,
    pub(crate) after: super::Instance,
}

pub(crate) fn row_resources_match(current: &super::Instance, expected: &super::Instance) -> bool {
    current.id == expected.id
        && current.created_at == expected.created_at
        && current.lifecycle_generation == expected.lifecycle_generation
        && current.scratch == expected.scratch
        && current.project_path == expected.project_path
        && current.worktree_info == expected.worktree_info
        && current.pre_trash_project_path == expected.pre_trash_project_path
        && current.trashed_at == expected.trashed_at
        && current.trash_lifecycle_id == expected.trash_lifecycle_id
        && current.workspace_info.as_ref().map(|workspace| {
            (
                &workspace.workspace_dir,
                &workspace.branch,
                &workspace.repos,
                workspace.created_at,
                workspace.cleanup_on_delete,
            )
        }) == expected.workspace_info.as_ref().map(|workspace| {
            (
                &workspace.workspace_dir,
                &workspace.branch,
                &workspace.repos,
                workspace.created_at,
                workspace.cleanup_on_delete,
            )
        })
}

pub(crate) fn row_matches_delta(current: &super::Instance, delta: &RowDelta, post: bool) -> bool {
    let expected = if post { &delta.after } else { &delta.before };
    row_resources_match(current, expected)
        && (current.lifecycle_reservation == expected.lifecycle_reservation
            || (!post
                && current.lifecycle_reservation.is_none()
                && expected.lifecycle_reservation_is_owned(
                    super::LifecycleOperation::Rename,
                    expected.lifecycle_generation,
                )))
        && (delta.before.title == delta.after.title || current.title == expected.title)
        && (delta.before.group_path == delta.after.group_path
            || current.group_path == expected.group_path)
        && (delta.before.tool == delta.after.tool || current.tool == expected.tool)
        && (delta.before.command == delta.after.command || current.command == expected.command)
        && (delta.before.extra_args == delta.after.extra_args
            || current.extra_args == expected.extra_args)
}

/// Removing a source copy must not discard an edit absent from its target.
pub(crate) fn source_changes_preserved(
    before: &super::Instance,
    source: &super::Instance,
    target: &super::Instance,
) -> Result<bool> {
    let before = serde_json::to_value(before)?;
    let source = serde_json::to_value(source)?;
    let target = serde_json::to_value(target)?;
    let before = before
        .as_object()
        .context("original session snapshot is not an object")?;
    let source = source
        .as_object()
        .context("session source snapshot is not an object")?;
    Ok(source
        .iter()
        .all(|(field, value)| before.get(field) == Some(value) || target.get(field) == Some(value))
        && before
            .keys()
            .filter(|field| !source.contains_key(*field))
            .all(|field| target.get(field).is_none()))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum EffectPhase {
    Prepared,
    Pending,
    Applied,
    Committed,
    Conflict,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum ResourceEffect {
    CreateWorktree {
        repository: PathBuf,
        path: PathBuf,
        branch: String,
        create_branch: bool,
        expected_detached: bool,
        base_branch: Option<String>,
        init_submodules: bool,
    },
    MoveWorktree {
        repository: PathBuf,
        from: PathBuf,
        to: PathBuf,
    },
    MoveDirectory {
        repository: Option<PathBuf>,
        from: PathBuf,
        to: PathBuf,
    },
    RenameBranch {
        repository: PathBuf,
        old: String,
        new: String,
        checkouts: Vec<PathBuf>,
    },
    RemoveWorktree {
        repository: PathBuf,
        path: PathBuf,
        force: bool,
    },
    DeleteBranch {
        repository: PathBuf,
        branch: String,
        checkouts: Vec<PathBuf>,
    },
    CreateBranchRef {
        repository: PathBuf,
        branch: String,
        tip: String,
        checkouts: Vec<PathBuf>,
    },
    CreateDirectory {
        path: PathBuf,
    },
    RemoveDirectory {
        path: PathBuf,
    },
    ReleaseContainer,
    PurgeAcpEvents {
        database: PathBuf,
    },
    PurgeSidecars {
        stores: Option<super::sandbox_store_reclaim::StoreRemoval>,
    },
    Callback {
        name: String,
        directory: PathBuf,
        checkouts: Vec<CallbackCheckout>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CallbackCheckout {
    pub(crate) repository: PathBuf,
    pub(crate) path: PathBuf,
    pub(crate) branch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct ReferenceObservation {
    pub(crate) repository: PathBuf,
    pub(crate) reference: String,
    pub(crate) tip: Option<String>,
    pub(crate) symbolic_target: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct CheckoutObservation {
    pub(crate) path: PathBuf,
    pub(crate) repository: Option<PathBuf>,
    pub(crate) exists: bool,
    pub(crate) registered: bool,
    pub(crate) branch: Option<String>,
    pub(crate) head: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct EffectObservation {
    pub(crate) references: Vec<ReferenceObservation>,
    pub(crate) checkouts: Vec<CheckoutObservation>,
    pub(crate) container_released: Option<bool>,
}

impl EffectObservation {
    fn checkout(&self, path: &Path) -> Result<&CheckoutObservation> {
        let path =
            super::deletion::resolve_claim_path(path).context("effect path cannot be resolved")?;
        self.checkouts
            .iter()
            .find(|checkout| checkout.path == path)
            .context("effect has no checkout observation")
    }

    fn reference(&self, branch: &str) -> Result<&ReferenceObservation> {
        self.references
            .iter()
            .find(|reference| reference.reference.strip_prefix("refs/heads/") == Some(branch))
            .context("effect has no branch observation")
    }
}

fn checkout_matches_creation(checkout: &CheckoutObservation, branch: &str, detached: bool) -> bool {
    checkout.exists
        && checkout.registered
        && checkout.head.is_some()
        && if detached {
            checkout.branch.is_none()
        } else {
            checkout
                .branch
                .as_deref()
                .and_then(|name| name.strip_prefix("refs/heads/"))
                == Some(branch)
        }
}

impl ResourceEffect {
    pub(crate) fn create_worktree(
        repository: PathBuf,
        path: PathBuf,
        branch: String,
        create_branch: bool,
        base_branch: Option<String>,
        init_submodules: bool,
    ) -> Result<Self> {
        let expected_detached = if create_branch {
            false
        } else {
            let repo = crate::git::open_repo_at(&repository)?;
            let parsed = repo.revparse_ext(&branch);
            match parsed {
                Ok((_, reference)) => reference.is_none_or(|reference| !reference.is_branch()),
                Err(error) if error.code() == git2::ErrorCode::NotFound => false,
                Err(error) => return Err(error.into()),
            }
        };
        Ok(Self::CreateWorktree {
            repository,
            path,
            branch,
            create_branch,
            expected_detached,
            base_branch,
            init_submodules,
        })
    }

    pub(crate) fn append_resource_keys(&self, keys: &mut Vec<ResourceKey>) -> Result<()> {
        match self {
            Self::CreateWorktree {
                repository,
                path,
                branch,
                expected_detached,
                ..
            } => {
                keys.push(ResourceKey::path(path, true)?);
                if !expected_detached {
                    keys.push(ResourceKey::branch(repository, branch)?);
                }
            }
            Self::MoveWorktree { from, to, .. } | Self::MoveDirectory { from, to, .. } => {
                keys.push(ResourceKey::path(from, true)?);
                keys.push(ResourceKey::path(to, true)?);
            }
            Self::RenameBranch {
                repository,
                old,
                new,
                checkouts,
            } => {
                keys.push(ResourceKey::branch(repository, old)?);
                keys.push(ResourceKey::branch(repository, new)?);
                for path in checkouts {
                    keys.push(ResourceKey::path(path, true)?);
                }
            }
            Self::RemoveWorktree { path, .. }
            | Self::CreateDirectory { path }
            | Self::RemoveDirectory { path } => keys.push(ResourceKey::path(path, true)?),
            Self::DeleteBranch {
                repository,
                branch,
                checkouts,
            }
            | Self::CreateBranchRef {
                repository,
                branch,
                checkouts,
                ..
            } => {
                keys.push(ResourceKey::branch(repository, branch)?);
                for path in checkouts {
                    keys.push(ResourceKey::path(path, true)?);
                }
            }
            Self::ReleaseContainer
            | Self::Callback { .. }
            | Self::PurgeAcpEvents { .. }
            | Self::PurgeSidecars { .. } => {}
        }
        Ok(())
    }

    /// Read Git metadata directly; observation never starts Git or a user hook.
    pub(crate) fn observe(&self) -> Result<EffectObservation> {
        match self {
            Self::CreateWorktree {
                repository,
                path,
                branch,
                expected_detached,
                ..
            } => observe_git(
                repository,
                &[(!expected_detached).then_some(branch.as_str())],
                [path.as_path()],
            ),
            Self::MoveWorktree {
                repository,
                from,
                to,
            } => observe_git(repository, &[], [from.as_path(), to.as_path()]),
            Self::MoveDirectory {
                repository,
                from,
                to,
            } => match repository {
                Some(repository) => observe_git(repository, &[], [from.as_path(), to.as_path()]),
                None => Ok(EffectObservation {
                    checkouts: vec![observe_directory(from)?, observe_directory(to)?],
                    ..EffectObservation::default()
                }),
            },
            Self::RenameBranch {
                repository,
                old,
                new,
                checkouts,
            } => observe_git(
                repository,
                &[Some(old.as_str()), Some(new.as_str())],
                checkouts.iter().map(PathBuf::as_path),
            ),
            Self::RemoveWorktree {
                repository, path, ..
            } => observe_git(repository, &[], [path.as_path()]),
            Self::DeleteBranch {
                repository,
                branch,
                checkouts,
            }
            | Self::CreateBranchRef {
                repository,
                branch,
                checkouts,
                ..
            } => observe_git(
                repository,
                &[Some(branch.as_str())],
                checkouts.iter().map(PathBuf::as_path),
            ),
            Self::ReleaseContainer | Self::PurgeAcpEvents { .. } | Self::PurgeSidecars { .. } => {
                Ok(EffectObservation::default())
            }
            Self::CreateDirectory { path } | Self::RemoveDirectory { path } => {
                Ok(EffectObservation {
                    references: Vec::new(),
                    checkouts: vec![observe_directory(path)?],
                    container_released: None,
                })
            }
            Self::Callback {
                directory,
                checkouts,
                ..
            } => {
                let mut observation = EffectObservation::default();
                for checkout in checkouts {
                    let part = observe_git(
                        &checkout.repository,
                        &[checkout.branch.as_deref()],
                        [checkout.path.as_path()],
                    )?;
                    observation.references.extend(part.references);
                    observation.checkouts.extend(part.checkouts);
                }
                let directory = observe_directory(directory)?;
                if !observation
                    .checkouts
                    .iter()
                    .any(|checkout| checkout.path == directory.path)
                {
                    observation.checkouts.push(directory);
                }
                Ok(observation)
            }
        }
    }

    /// Replay only declared local effects. User callbacks require their producer.
    pub(crate) fn execute(&self) -> Result<Vec<String>> {
        match self {
            Self::CreateWorktree {
                repository,
                path,
                branch,
                create_branch,
                base_branch,
                init_submodules,
                ..
            } => {
                return Ok(crate::git::GitWorktree::new(repository.clone())?
                    .with_init_submodules(*init_submodules)
                    .create_worktree(branch, path, *create_branch, base_branch.as_deref())?);
            }
            Self::MoveWorktree {
                repository,
                from,
                to,
            } => {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)?;
                }
                crate::git::GitWorktree::new(repository.clone())?.move_worktree(from, to)?;
            }
            Self::MoveDirectory { from, to, .. } => {
                anyhow::ensure!(
                    !to.try_exists()?,
                    "directory destination already exists: {}",
                    to.display()
                );
                fs::rename(from, to)
                    .with_context(|| format!("moving {} to {}", from.display(), to.display()))?;
            }
            Self::RenameBranch {
                repository,
                old,
                new,
                ..
            } => {
                crate::git::GitWorktree::new(repository.clone())?.rename_branch(old, new)?;
            }
            Self::RemoveWorktree {
                repository,
                path,
                force,
            } => {
                crate::git::GitWorktree::new(repository.clone())?.remove_worktree(path, *force)?;
            }
            Self::DeleteBranch {
                repository, branch, ..
            } => {
                crate::git::GitWorktree::new(repository.clone())?.delete_branch(branch)?;
            }
            Self::CreateBranchRef {
                repository,
                branch,
                tip,
                ..
            } => {
                crate::git::GitWorktree::new(repository.clone())?
                    .create_branch_ref_if_absent(branch, tip)?;
            }
            Self::CreateDirectory { path } => fs::create_dir_all(path)
                .with_context(|| format!("creating resource directory {}", path.display()))?,
            Self::RemoveDirectory { path } => match fs::remove_dir_all(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            },
            Self::ReleaseContainer
            | Self::Callback { .. }
            | Self::PurgeAcpEvents { .. }
            | Self::PurgeSidecars { .. } => {
                anyhow::bail!("this external effect requires its original producer")
            }
        }
        Ok(Vec::new())
    }

    pub(crate) fn goal_reached(&self, after: &EffectObservation) -> Result<bool> {
        Ok(match self {
            Self::CreateWorktree {
                path,
                branch,
                expected_detached,
                ..
            } => checkout_matches_creation(after.checkout(path)?, branch, *expected_detached),
            Self::MoveWorktree { from, to, .. } => {
                let original = after.checkout(from)?;
                let destination = after.checkout(to)?;
                !original.exists
                    && !original.registered
                    && destination.exists
                    && destination.registered
            }
            Self::MoveDirectory { from, to, .. } => {
                !after.checkout(from)?.exists && after.checkout(to)?.exists
            }
            Self::RenameBranch { old, new, .. } => {
                let original = after.reference(old)?;
                let renamed = after.reference(new)?;
                original.tip.is_none()
                    && original.symbolic_target.is_none()
                    && (renamed.tip.is_some() || renamed.symbolic_target.is_some())
                    && after.checkouts.iter().all(|checkout| {
                        checkout
                            .branch
                            .as_deref()
                            .and_then(|name| name.strip_prefix("refs/heads/"))
                            == Some(new.as_str())
                    })
            }
            Self::RemoveWorktree { path, .. } => {
                let checkout = after.checkout(path)?;
                !checkout.exists && !checkout.registered
            }
            Self::DeleteBranch { branch, .. } => {
                let reference = after.reference(branch)?;
                reference.tip.is_none() && reference.symbolic_target.is_none()
            }
            Self::CreateBranchRef { branch, tip, .. } => {
                let reference = after.reference(branch)?;
                reference.tip.as_ref() == Some(tip) && reference.symbolic_target.is_none()
            }
            Self::CreateDirectory { path } => after.checkout(path)?.exists,
            Self::RemoveDirectory { path } => !after.checkout(path)?.exists,
            Self::ReleaseContainer => after.container_released == Some(true),
            Self::Callback { .. } | Self::PurgeAcpEvents { .. } | Self::PurgeSidecars { .. } => {
                true
            }
        })
    }
}

fn observe_directory(path: &Path) -> Result<CheckoutObservation> {
    Ok(CheckoutObservation {
        path: super::deletion::resolve_claim_path(path)
            .context("directory effect path cannot be resolved")?,
        exists: path.try_exists()?,
        repository: None,
        registered: false,
        branch: None,
        head: None,
    })
}

fn observe_git<'a>(
    repository: &Path,
    branches: &[Option<&str>],
    paths: impl IntoIterator<Item = &'a Path>,
) -> Result<EffectObservation> {
    let repo = crate::git::open_repo_at(repository)?;
    let mut references = Vec::with_capacity(branches.iter().flatten().count());
    if branches.iter().any(Option::is_some) {
        let common = fs::canonicalize(repo.commondir())?;
        for branch in branches.iter().flatten() {
            let reference = format!("refs/heads/{branch}");
            let (tip, symbolic_target) = observe_reference(&repo, &reference)?;
            references.push(ReferenceObservation {
                repository: common.clone(),
                reference,
                tip,
                symbolic_target,
            });
        }
    }
    let paths = paths.into_iter();
    let mut checkouts = Vec::with_capacity(paths.size_hint().0);
    for path in paths {
        checkouts.push(observe_checkout(&repo, path)?);
    }
    Ok(EffectObservation {
        references,
        checkouts,
        container_released: None,
    })
}

fn observe_reference(
    repo: &git2::Repository,
    name: &str,
) -> Result<(Option<String>, Option<String>)> {
    let reference = match repo.find_reference(name) {
        Ok(reference) => reference,
        Err(error) if error.code() == git2::ErrorCode::NotFound => return Ok((None, None)),
        Err(error) => return Err(error.into()),
    };
    let symbolic_target = reference.symbolic_target()?.map(str::to_owned);
    let tip = match reference.resolve() {
        Ok(resolved) => resolved.target().map(|oid| oid.to_string()),
        Err(error)
            if matches!(
                error.code(),
                git2::ErrorCode::NotFound | git2::ErrorCode::UnbornBranch
            ) =>
        {
            None
        }
        Err(error) => return Err(error.into()),
    };
    Ok((tip, symbolic_target))
}

fn observe_checkout(repo: &git2::Repository, path: &Path) -> Result<CheckoutObservation> {
    let resolved = super::deletion::resolve_claim_path(path)
        .context("checkout observation path cannot be resolved")?;
    let mut registered = repo
        .workdir()
        .is_some_and(|main| super::deletion::resolve_claim_path(main).as_ref() == Some(&resolved));
    let mut registered_head = registered
        .then(|| observe_reference(repo, "HEAD"))
        .transpose()?;
    if !registered {
        for name in repo.worktrees()?.iter() {
            let name = name?.context("worktree name is not UTF-8")?;
            let worktree = repo.find_worktree(name)?;
            if super::deletion::resolve_claim_path(worktree.path()).as_ref() == Some(&resolved) {
                registered = true;
                let checkout =
                    git2::Repository::open(repo.commondir().join("worktrees").join(name))?;
                registered_head = Some(observe_reference(&checkout, "HEAD")?);
                break;
            }
        }
    }
    let exists = path.try_exists()?;
    let (head, branch) = if let Some(head) = registered_head {
        head
    } else if exists {
        match crate::git::open_repo_at(path) {
            Ok(checkout) => observe_reference(&checkout, "HEAD")?,
            Err(error) if error.code() == git2::ErrorCode::NotFound => (None, None),
            Err(error) => return Err(error.into()),
        }
    } else {
        (None, None)
    };
    Ok(CheckoutObservation {
        path: resolved,
        repository: Some(fs::canonicalize(repo.commondir())?),
        exists,
        registered,
        branch,
        head,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct EffectRecord {
    pub(crate) effect: ResourceEffect,
    pub(crate) phase: EffectPhase,
    pub(crate) before: Option<EffectObservation>,
    pub(crate) after: Option<EffectObservation>,
    pub(crate) error: Option<String>,
    pub(crate) compensation: bool,
    pub(crate) retained: bool,
}

/// Pending effects remain reserved until an operator attests that all actors stopped.
/// Snapshot equality or a free parent lease never proves a child has exited.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ResourceIntent {
    pub(crate) owner: IntentOwner,
    pub(crate) target: ProfileAuthority,
    pub(crate) operation: super::LifecycleOperation,
    /// Creation has no before row; irreversible purge has no after row.
    pub(crate) before: Option<super::Instance>,
    pub(crate) after: Option<super::Instance>,
    pub(crate) resources: Vec<ResourceKey>,
    pub(crate) effects: Vec<EffectRecord>,
    pub(crate) phase: EffectPhase,
    pub(crate) quiesced: bool,
    pub(crate) rollback: bool,
    #[serde(default)]
    pub(crate) abandoned: bool,
    pub(crate) conflict: Option<String>,
}

impl ResourceIntent {
    pub(crate) fn row_is_reserved(
        &self,
        profile: &ProfileAuthority,
        row: &super::Instance,
        journal: &str,
    ) -> bool {
        self.owner.matches(profile, row)
            && row.lifecycle_reservation_is_owned(self.operation, self.owner.generation)
            && row
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|reservation| reservation.resource_intent.as_deref() == Some(journal))
            && self
                .before
                .iter()
                .chain(&self.after)
                .any(|expected| same_namespace(row, expected))
    }

    pub(crate) fn new(
        owner: IntentOwner,
        target: ProfileAuthority,
        operation: super::LifecycleOperation,
        mut before: Option<super::Instance>,
        mut after: Option<super::Instance>,
        effects: Vec<ResourceEffect>,
        mut resources: Vec<ResourceKey>,
    ) -> Result<Self> {
        anyhow::ensure!(
            before.is_some() || after.is_some(),
            "resource intent has no session incarnation"
        );
        for row in before.iter().chain(&after) {
            anyhow::ensure!(
                row.id == owner.id && row.created_at == owner.created_at,
                "resource intent mixes session incarnations"
            );
        }
        if let Some(after) = &after {
            anyhow::ensure!(
                after.lifecycle_generation == owner.generation,
                "intent post-generation differs from its owner"
            );
        }
        // Routing belongs to the intent, not the transient UI-only row field.
        for row in before.iter_mut().chain(&mut after) {
            row.source_profile.clear();
        }
        for effect in &effects {
            effect.append_resource_keys(&mut resources)?;
        }
        resources.sort_unstable();
        resources.dedup();
        Ok(Self {
            owner,
            target,
            operation,
            before,
            after,
            resources,
            effects: effects
                .into_iter()
                .map(|effect| EffectRecord {
                    effect,
                    phase: EffectPhase::Prepared,
                    before: None,
                    after: None,
                    error: None,
                    compensation: false,
                    retained: false,
                })
                .collect(),
            phase: EffectPhase::Prepared,
            quiesced: false,
            rollback: false,
            abandoned: false,
            conflict: None,
        })
    }

    pub(crate) fn effect_goal_reached(&self, record: &EffectRecord) -> Result<bool> {
        let after = record
            .after
            .as_ref()
            .context("effect has no completed observation")?;
        if let ResourceEffect::CreateWorktree {
            path,
            branch,
            expected_detached,
            ..
        } = &record.effect
        {
            let original = after.checkout(path)?;
            let latest = self
                .effects
                .iter()
                .rev()
                .filter(|effect| effect.phase == EffectPhase::Applied)
                .filter_map(|effect| effect.after.as_ref())
                .flat_map(|after| after.checkouts.iter())
                .find(|checkout| checkout.path == original.path)
                .unwrap_or(original);
            Ok(checkout_matches_creation(
                latest,
                branch,
                *expected_detached,
            ))
        } else {
            record.effect.goal_reached(after)
        }
    }

    /// Choose an inverse from retained authority; current reads only select its state.
    pub(crate) fn next_compensation(&self) -> Result<Option<ResourceEffect>> {
        anyhow::ensure!(self.rollback, "operation has no rollback decision");
        self.verify_after(false)?;
        for record in self
            .effects
            .iter()
            .rev()
            .filter(|record| !record.compensation && !record.retained)
        {
            if record.phase == EffectPhase::Prepared && record.before.is_none() {
                continue;
            }
            let before = record
                .before
                .as_ref()
                .context("effect has no original observation")?;
            let after = record
                .after
                .as_ref()
                .context("effect has no completed observation")?;
            if before == after {
                if let ResourceEffect::RemoveWorktree { path, .. }
                | ResourceEffect::RemoveDirectory { path } = &record.effect
                {
                    anyhow::ensure!(!before.checkout(path)?.exists,
                        "cleanup may have removed directory contents; manual conflict resolution is required");
                }
                continue;
            }
            match &record.effect {
                ResourceEffect::Callback { .. }
                | ResourceEffect::ReleaseContainer
                | ResourceEffect::PurgeAcpEvents { .. }
                | ResourceEffect::PurgeSidecars { .. } => continue,
                ResourceEffect::CreateWorktree {
                    repository,
                    path,
                    branch,
                    create_branch,
                    ..
                } => {
                    anyhow::ensure!(
                        !before.checkout(path)?.exists,
                        "creation did not allocate this checkout"
                    );
                    let current = record.effect.observe()?;
                    let checkout = current.checkout(path)?;
                    if checkout.registered {
                        return Ok(Some(ResourceEffect::RemoveWorktree {
                            repository: repository.clone(),
                            path: path.clone(),
                            force: false,
                        }));
                    }
                    if checkout.exists {
                        return Ok(Some(ResourceEffect::RemoveDirectory { path: path.clone() }));
                    }
                    if *create_branch
                        && before.reference(branch)?.tip.is_none()
                        && before.reference(branch)?.symbolic_target.is_none()
                    {
                        let reference = current.reference(branch)?;
                        if reference.tip.is_some() || reference.symbolic_target.is_some() {
                            let git = crate::git::GitWorktree::new(repository.clone())?;
                            return Ok(Some(ResourceEffect::DeleteBranch {
                                repository: repository.clone(),
                                branch: branch.clone(),
                                checkouts: git
                                    .list_worktrees()?
                                    .into_iter()
                                    .filter(|checkout| {
                                        checkout.branch.as_deref() == Some(branch.as_str())
                                    })
                                    .map(|checkout| checkout.path)
                                    .collect(),
                            }));
                        }
                    }
                }
                ResourceEffect::MoveWorktree {
                    repository,
                    from,
                    to,
                } => {
                    let current = record.effect.observe()?;
                    let original = current.checkout(from)?;
                    let destination = current.checkout(to)?;
                    anyhow::ensure!(
                        !before.checkout(to)?.exists,
                        "move target was not originally free"
                    );
                    if original.exists
                        && original.registered
                        && !destination.exists
                        && !destination.registered
                    {
                        continue;
                    }
                    anyhow::ensure!(
                        !original.exists && destination.exists,
                        "worktree move has an ambiguous result"
                    );
                    return Ok(Some(if destination.registered {
                        ResourceEffect::MoveWorktree {
                            repository: repository.clone(),
                            from: to.clone(),
                            to: from.clone(),
                        }
                    } else {
                        anyhow::ensure!(
                            original.registered,
                            "original worktree registration was lost"
                        );
                        ResourceEffect::MoveDirectory {
                            repository: Some(repository.clone()),
                            from: to.clone(),
                            to: from.clone(),
                        }
                    }));
                }
                ResourceEffect::MoveDirectory {
                    repository,
                    from,
                    to,
                } => {
                    let current = record.effect.observe()?;
                    let original = current.checkout(from)?;
                    let destination = current.checkout(to)?;
                    anyhow::ensure!(
                        !before.checkout(to)?.exists,
                        "move target was not originally free"
                    );
                    if original.exists && !destination.exists {
                        continue;
                    }
                    anyhow::ensure!(
                        !original.exists && destination.exists,
                        "directory move has an ambiguous result"
                    );
                    return Ok(Some(ResourceEffect::MoveDirectory {
                        repository: repository.clone(),
                        from: to.clone(),
                        to: from.clone(),
                    }));
                }
                ResourceEffect::RenameBranch {
                    repository,
                    old,
                    new,
                    checkouts,
                } => {
                    let current = record.effect.observe()?;
                    let original = current.reference(old)?;
                    let renamed = current.reference(new)?;
                    let original_present =
                        original.tip.is_some() || original.symbolic_target.is_some();
                    let renamed_present =
                        renamed.tip.is_some() || renamed.symbolic_target.is_some();
                    let bindings_are_original = current.checkouts.iter().all(|checkout| {
                        checkout
                            .branch
                            .as_deref()
                            .and_then(|name| name.strip_prefix("refs/heads/"))
                            == Some(old.as_str())
                    });
                    let target_existed = before.reference(new)?.tip.is_some()
                        || before.reference(new)?.symbolic_target.is_some();
                    if original_present
                        && bindings_are_original
                        && (target_existed || !renamed_present)
                    {
                        continue;
                    }
                    anyhow::ensure!(!target_existed, "rename target was not originally free");
                    if !original_present && renamed_present {
                        return Ok(Some(ResourceEffect::RenameBranch {
                            repository: repository.clone(),
                            old: new.clone(),
                            new: old.clone(),
                            checkouts: checkouts.clone(),
                        }));
                    }
                    anyhow::ensure!(
                        !original_present && !renamed_present,
                        "branch rename has conflicting owners"
                    );
                    anyhow::ensure!(self.quiesced, "a lost live branch tip cannot be reconstructed from an earlier observation");
                    anyhow::ensure!(
                        current.checkouts.iter().all(|checkout| {
                            checkout
                                .branch
                                .as_deref()
                                .and_then(|name| name.strip_prefix("refs/heads/"))
                                .is_some_and(|branch| branch == old || branch == new)
                        }),
                        "checkout binding changed during branch rename"
                    );
                    let restore = if current.checkouts.iter().any(|checkout| {
                        checkout
                            .branch
                            .as_deref()
                            .and_then(|name| name.strip_prefix("refs/heads/"))
                            == Some(new.as_str())
                    }) {
                        new
                    } else {
                        old
                    };
                    return Ok(Some(ResourceEffect::CreateBranchRef {
                        repository: repository.clone(),
                        branch: restore.clone(),
                        tip: self
                            .last_rename_tip(&before.reference(old)?.repository, old, new)?
                            .to_owned(),
                        checkouts: checkouts.clone(),
                    }));
                }
                ResourceEffect::CreateDirectory { path } => {
                    if !before.checkout(path)?.exists
                        && record.effect.observe()?.checkout(path)?.exists
                    {
                        return Ok(Some(ResourceEffect::RemoveDirectory { path: path.clone() }));
                    }
                }
                ResourceEffect::RemoveWorktree { .. }
                | ResourceEffect::DeleteBranch { .. }
                | ResourceEffect::CreateBranchRef { .. }
                | ResourceEffect::RemoveDirectory { .. } => {
                    anyhow::bail!("an irreversible removal cannot be rolled back")
                }
            }
        }
        Ok(None)
    }

    fn last_rename_tip(&self, repository: &Path, old: &str, new: &str) -> Result<&str> {
        for record in self.effects.iter().rev() {
            for observation in record.after.iter().chain(&record.before) {
                if let Some(reference) = observation.references.iter().find(|reference| {
                    reference.repository == repository
                        && reference
                            .reference
                            .strip_prefix("refs/heads/")
                            .is_some_and(|branch| branch == old || branch == new)
                        && (reference.tip.is_some() || reference.symbolic_target.is_some())
                }) {
                    anyhow::ensure!(
                        reference.symbolic_target.is_none(),
                        "a symbolic branch cannot be reconstructed as a direct ref"
                    );
                    return reference
                        .tip
                        .as_deref()
                        .context("completed effect retained no branch tip");
                }
            }
        }
        anyhow::bail!("no original completed effect retained a branch tip")
    }

    /// Publication checks names; compensation also requires the recorded tips.
    pub(crate) fn verify_after(&self, exact_tips: bool) -> Result<()> {
        let mut references = std::collections::BTreeMap::new();
        let mut checkouts = std::collections::BTreeMap::new();
        let mut removed_directories = std::collections::BTreeMap::new();
        for (index, record) in self.effects.iter().enumerate() {
            anyhow::ensure!(
                !matches!(record.phase, EffectPhase::Pending | EffectPhase::Conflict),
                "an external effect has no authoritative completed observation"
            );
            let Some(after) = &record.after else { continue };
            for reference in &after.references {
                references.insert(
                    (reference.repository.as_path(), reference.reference.as_str()),
                    (reference, index),
                );
            }
            for checkout in &after.checkouts {
                checkouts.insert(checkout.path.as_path(), (checkout, index));
            }
            if let ResourceEffect::RemoveDirectory { path } = &record.effect {
                if after.checkouts.iter().any(|directory| !directory.exists) {
                    removed_directories.insert(path.as_path(), index);
                }
            }
        }
        let repository_removed = |path: &Path, observed_at: usize| {
            path.ancestors().any(|ancestor| {
                removed_directories
                    .get(ancestor)
                    .is_some_and(|removed_at| *removed_at > observed_at)
            })
        };
        let tip_matches = |expected: &Option<String>, actual: &Option<String>| {
            expected == actual || (!exact_tips && expected.is_some() && actual.is_some())
        };
        for ((repository, reference), (expected, index)) in references {
            if repository_removed(repository, index) {
                anyhow::ensure!(!repository.try_exists()?, "removed repository reappeared");
                continue;
            }
            let repo = crate::git::open_repo_at(repository)?;
            let (tip, symbolic_target) = observe_reference(&repo, reference)?;
            anyhow::ensure!(
                tip_matches(&expected.tip, &tip) && expected.symbolic_target == symbolic_target,
                "branch changed after its recorded effect: {reference}"
            );
        }
        for (path, (expected, index)) in checkouts {
            if let Some(repository) = &expected.repository {
                if repository_removed(repository, index) {
                    anyhow::ensure!(
                        !expected.exists && !path.try_exists()?,
                        "a checkout survived removal of its repository"
                    );
                    continue;
                }
                let repo = crate::git::open_repo_at(repository)?;
                let actual = observe_checkout(&repo, path)?;
                anyhow::ensure!(
                    actual.path == expected.path
                        && actual.repository == expected.repository
                        && actual.exists == expected.exists
                        && actual.registered == expected.registered
                        && actual.branch == expected.branch
                        && tip_matches(&expected.head, &actual.head),
                    "checkout changed after its recorded effect: {}",
                    path.display()
                );
            } else {
                anyhow::ensure!(
                    observe_directory(path)? == *expected,
                    "directory changed after its recorded effect: {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct JournalScope {
    pub(crate) profiles: Vec<ProfileAuthority>,
    pub(crate) resources: Vec<ResourceKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum MoveAuthority {
    Metadata {
        source: ProfileAuthority,
        target: ProfileAuthority,
        rows: Vec<RowDelta>,
    },
    Resource {
        intent: Box<ResourceIntent>,
    },
    LegacyConflict {
        legacy: serde_json::Value,
        reason: String,
        scope: JournalScope,
    },
    Cleanup {
        scope: JournalScope,
        effects: Vec<EffectRecord>,
        #[serde(default)]
        abandoned: bool,
        conflict: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MoveJournalEntry {
    pub(crate) version: u32,
    /// Sorted session ids covered by one move batch.
    pub(crate) ids: Vec<String>,
    pub(crate) source_profile: String,
    pub(crate) target_profile: String,
    /// Absolute paths of both profiles' `sessions.json`, as recorded at
    /// journal-write time. Recovery re-checks them against the live stores.
    pub(crate) source_sessions_path: PathBuf,
    pub(crate) target_sessions_path: PathBuf,
    pub(crate) group_move_source_path: String,
    pub(crate) group_move_target_path: String,
    pub(crate) group_move_subtree: bool,
    pub(crate) created_at_epoch_ms: u64,
    pub(crate) authority: MoveAuthority,
}

impl MoveJournalEntry {
    pub(crate) fn is_current(&self) -> bool {
        self.version == MOVE_JOURNAL_VERSION
    }
}

/// The journal is the only cross-process resource reservation inventory.
pub(crate) fn resource_inventory() -> Result<Vec<(PathBuf, MoveJournalEntry)>> {
    let scanned = scan();
    anyhow::ensure!(
        scanned.unreadable_dirs.is_empty(),
        "resource intent inventory is incomplete: {:?}; inspect with `aoe intents list`; use `aoe intents clear '<path>' --actors-stopped` after resolving the named directory",
        scanned.unreadable_dirs
    );
    scanned
        .entries
        .into_iter()
        .map(|(path, entry)| {
            let entry = entry.map_err(|error| anyhow::anyhow!(
                "{}: {error}; inspect with `aoe intents list`; resolve with `aoe intents clear {} --actors-stopped`",
                path.display(), shell_escape_script_word(&path.to_string_lossy())
            ))?;
            Ok((path, entry))
        })
        .collect()
}

impl MoveJournalEntry {
    pub(crate) fn affects_profile(&self, profile: &ProfileAuthority) -> bool {
        self.source_profile == profile.profile
            || self.target_profile == profile.profile
            || self.source_sessions_path == profile.sessions_path
            || self.target_sessions_path == profile.sessions_path
            || match &self.authority {
                MoveAuthority::Resource { intent } => {
                    intent.owner.profile.sessions_path == profile.sessions_path
                        || intent.target.sessions_path == profile.sessions_path
                }
                MoveAuthority::Metadata { source, target, .. } => {
                    source.sessions_path == profile.sessions_path
                        || target.sessions_path == profile.sessions_path
                }
                MoveAuthority::LegacyConflict { scope, .. }
                | MoveAuthority::Cleanup { scope, .. } => scope.profiles.iter().any(|known| {
                    known.sessions_path == profile.sessions_path || known.profile == profile.profile
                }),
            }
    }

    pub(crate) fn reserved_resources(&self) -> &[ResourceKey] {
        match &self.authority {
            MoveAuthority::Resource { intent } => &intent.resources,
            MoveAuthority::LegacyConflict { scope, .. } | MoveAuthority::Cleanup { scope, .. } => {
                &scope.resources
            }
            MoveAuthority::Metadata { .. } => &[],
        }
    }

    pub(crate) fn is_arbitration_evidence(&self) -> bool {
        matches!(
            self.authority,
            MoveAuthority::Metadata { .. } | MoveAuthority::LegacyConflict { .. }
        )
    }
}

fn has_recorded_intents() -> Result<bool> {
    let directory = journal_dir()?;
    anyhow::ensure!(
        !fs::symlink_metadata(&directory).is_ok_and(|metadata| metadata.file_type().is_symlink()),
        "journal directory is an alias"
    );
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            anyhow::ensure!(
                matches!(fs::symlink_metadata(&directory), Err(error) if error.kind() == std::io::ErrorKind::NotFound),
                "journal directory is an unreadable alias"
            );
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        if entry?
            .path()
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Generic updates may annotate reserved rows, but cannot adopt or retire their resources.
pub(crate) struct ResourceAdmission<'a> {
    before: Option<Vec<RowClaims>>,
    storage: &'a super::Storage,
    owner: Option<&'a JournalTransaction>,
    attested: Option<&'a super::Instance>,
}

impl<'a> ResourceAdmission<'a> {
    pub(crate) fn capture(
        storage: &'a super::Storage,
        rows: &[super::Instance],
        owner: Option<&'a JournalTransaction>,
    ) -> Self {
        // Failure to prove absence requires a projection, not an unsafe fast path.
        let needed = owner.is_some()
            || has_recorded_intents().unwrap_or(true)
            || rows.iter().any(|row| {
                row.lifecycle_reservation
                    .as_ref()
                    .is_some_and(|reservation| reservation.resource_intent.is_some())
            });
        Self {
            before: needed.then(|| rows.iter().map(RowClaims::capture).collect()),
            storage,
            owner,
            attested: None,
        }
    }

    pub(crate) fn capture_attested(
        storage: &'a super::Storage,
        rows: &[super::Instance],
        expected: &'a super::Instance,
    ) -> Self {
        Self {
            before: Some(rows.iter().map(RowClaims::capture).collect()),
            storage,
            owner: None,
            attested: Some(expected),
        }
    }

    fn clears_attested_binding(
        &self,
        old: Option<&RowClaims>,
        row: Option<&super::Instance>,
    ) -> bool {
        let (Some(expected), Some(old), Some(row)) = (self.attested, old, row) else {
            return false;
        };
        old.matches(expected)
            && expected
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|reservation| {
                    reservation.resource_intent.is_some()
                        && reservation.generation == expected.lifecycle_generation
                })
            && row.id == old.id
            && row.created_at == old.created_at
            && row.lifecycle_generation == old.generation
            && row.lifecycle_reservation.is_none()
            && old.namespace_matches(row)
    }

    pub(crate) fn validate(&self, rows: &[super::Instance]) -> Result<()> {
        let Some(before) = &self.before else {
            anyhow::ensure!(
                rows.iter().all(|row| row
                    .lifecycle_reservation
                    .as_ref()
                    .is_none_or(|reservation| reservation.resource_intent.is_none())),
                "a resource reservation requires its owning transaction"
            );
            return Ok(());
        };
        let mut remaining: std::collections::HashMap<_, _> = before
            .iter()
            .map(|row| ((row.id.as_str(), row.created_at), row))
            .collect();
        let mut changes = Vec::new();
        for row in rows {
            let old = remaining.remove(&(row.id.as_str(), row.created_at));
            if old.is_none_or(|old| !old.matches(row)) {
                changes.push((old, Some(row), false));
            }
        }
        changes.extend(remaining.into_values().map(|old| (Some(old), None, false)));
        if changes.is_empty() {
            return Ok(());
        }
        let profile = ProfileAuthority::capture(self.storage)?;
        let inventory = resource_inventory()?;
        let owner = self
            .owner
            .map(|transaction| transaction.read().map(|entry| (transaction, entry)))
            .transpose()?;
        let mut all_owned = true;
        for (old, row, owns) in &mut changes {
            *owns = owner.as_ref().is_some_and(|(transaction, entry)| {
                owns_row_change(transaction, entry, &profile, *old, *row)
            });
            all_owned &= *owns;
        }
        let owner_path = owner.as_ref().map(|(transaction, _)| &transaction.path);
        let reservations = inventory
            .iter()
            .filter(|(path, _)| !all_owned || Some(path) != owner_path);
        for (old, row, owns) in changes {
            let old_intent = old.and_then(RowClaims::resource_intent);
            let new_intent = row
                .and_then(|row| row.lifecycle_reservation.as_ref())
                .and_then(|reservation| reservation.resource_intent.as_deref());
            anyhow::ensure!(
                owns || self.clears_attested_binding(old, row)
                    || (old_intent.is_none() && new_intent.is_none()),
                "resource reservation is busy; only its owning transaction can change it"
            );
            // A ticket cannot bypass its own record for a different row or generation.
            if let Some((transaction, entry)) = &owner {
                if entry.ids.iter().any(|id| {
                    old.is_some_and(|old| old.id == *id) || row.is_some_and(|row| row.id == *id)
                }) && entry.affects_profile(&profile)
                {
                    anyhow::ensure!(
                        owns,
                        "resource transaction owner was superseded: {}",
                        transaction.id()
                    );
                }
            }
            let id = row
                .map(|row| row.id.as_str())
                .or_else(|| old.map(|old| old.id.as_str()))
                .expect("a change contains at least one row");
            for (path, entry) in reservations.clone() {
                if owns && Some(path) == owner_path {
                    continue;
                }
                anyhow::ensure!(
                    !entry.affects_profile(&profile) || !entry.ids.iter().any(|known| known == id),
                    "session resource is busy in {}",
                    path.display()
                );
            }
            if reservations
                .clone()
                .all(|(path, _)| owns && Some(path) == owner_path)
            {
                continue;
            }
            let include_branches = old
                .zip(row)
                .is_none_or(|(old, row)| !old.branch_claims_match(row))
                && reservations.clone().any(|(path, entry)| {
                    !(owns && Some(path) == owner_path)
                        && entry
                            .reserved_resources()
                            .iter()
                            .any(|key| matches!(key, ResourceKey::Branch { .. }))
                });
            let mut keys = match old {
                Some(old) => old.keys(include_branches)?,
                None => Vec::new(),
            };
            if let Some(row) = row {
                keys.extend(row_keys(row, include_branches)?);
            }
            for (path, entry) in reservations.clone() {
                if owns && Some(path) == owner_path {
                    continue;
                }
                let reserved = entry.reserved_resources();
                anyhow::ensure!(
                    !keys
                        .iter()
                        .any(|key| reserved.iter().any(|held| key.overlaps(held))),
                    "checkout resource is busy in {}",
                    path.display()
                );
            }
        }
        Ok(())
    }
}

fn owns_row_change(
    transaction: &JournalTransaction,
    entry: &MoveJournalEntry,
    profile: &ProfileAuthority,
    before: Option<&RowClaims>,
    after: Option<&super::Instance>,
) -> bool {
    let valid_identity = |id: &str, created_at, generation, owner: &IntentOwner| {
        id == owner.id && created_at == owner.created_at && generation == owner.generation
    };
    match &entry.authority {
        MoveAuthority::LegacyConflict { .. } | MoveAuthority::Cleanup { .. } => false,
        MoveAuthority::Metadata {
            source,
            target,
            rows,
        } => {
            if profile.sessions_path != source.sessions_path
                && profile.sessions_path != target.sessions_path
            {
                return false;
            }
            rows.iter().any(|delta| {
                before.is_none_or(|old| {
                    old.id == delta.before.id
                        && old.created_at == delta.before.created_at
                        && old.generation == delta.before.lifecycle_generation
                        && (old.namespace_matches(&delta.before)
                            || old.namespace_matches(&delta.after))
                }) && after.is_none_or(|row| {
                    row.id == delta.after.id
                        && row.created_at == delta.after.created_at
                        && row.lifecycle_generation == delta.after.lifecycle_generation
                        && same_namespace(row, &delta.after)
                })
            })
        }
        MoveAuthority::Resource { intent } => {
            if (!intent.abandoned
                && matches!(intent.phase, EffectPhase::Pending | EffectPhase::Conflict))
                || (profile.sessions_path != intent.owner.profile.sessions_path
                    && profile.sessions_path != intent.target.sessions_path)
            {
                return false;
            }
            let prepared = intent.phase == EffectPhase::Prepared
                && intent
                    .effects
                    .iter()
                    .all(|effect| effect.phase == EffectPhase::Prepared);
            if let Some(old) = before {
                let initial = prepared
                    && intent.before.as_ref().is_some_and(|original| {
                        old.id == original.id
                            && old.created_at == original.created_at
                            && old.generation == original.lifecycle_generation
                            && old.namespace_matches(original)
                            && old.resource_intent().is_none()
                    });
                let owned = valid_identity(&old.id, old.created_at, old.generation, &intent.owner)
                    && old.resource_intent() == Some(transaction.id())
                    && intent
                        .before
                        .iter()
                        .chain(&intent.after)
                        .any(|expected| old.namespace_matches(expected));
                if !initial && !owned {
                    return false;
                }
            }
            if let Some(row) = after {
                if !valid_identity(
                    &row.id,
                    row.created_at,
                    row.lifecycle_generation,
                    &intent.owner,
                ) || !intent
                    .before
                    .iter()
                    .chain(&intent.after)
                    .any(|expected| same_namespace(row, expected))
                {
                    return false;
                }
                let marker = row
                    .lifecycle_reservation
                    .as_ref()
                    .and_then(|reservation| reservation.resource_intent.as_deref());
                if marker == Some(transaction.id()) {
                    return true;
                }
                marker.is_none()
                    && (intent.abandoned
                        || matches!(intent.phase, EffectPhase::Applied | EffectPhase::Committed)
                        || (prepared
                            && (intent.effects.is_empty()
                                || intent
                                    .before
                                    .as_ref()
                                    .is_some_and(|original| same_namespace(row, original)))))
            } else {
                intent.after.is_none()
                    || intent.before.is_none()
                    || intent.operation == super::LifecycleOperation::Create
                    || intent.owner.profile.sessions_path != intent.target.sessions_path
            }
        }
    }
}

pub(crate) fn same_namespace(left: &super::Instance, right: &super::Instance) -> bool {
    left.scratch == right.scratch
        && left.project_path == right.project_path
        && left.pre_trash_project_path == right.pre_trash_project_path
        && left.trashed_at == right.trashed_at
        && left.trash_lifecycle_id == right.trash_lifecycle_id
        && match (&left.worktree_info, &right.worktree_info) {
            (Some(left), Some(right)) => {
                left.branch == right.branch
                    && left.main_repo_path == right.main_repo_path
                    && left.managed_by_aoe == right.managed_by_aoe
                    && left.created_at == right.created_at
            }
            (None, None) => true,
            _ => false,
        }
        && match (&left.workspace_info, &right.workspace_info) {
            (Some(left), Some(right)) => {
                left.workspace_dir == right.workspace_dir
                    && left.branch == right.branch
                    && left.cleanup_on_delete == right.cleanup_on_delete
                    && left.repos.len() == right.repos.len()
                    && left.repos.iter().zip(&right.repos).all(|(left, right)| {
                        left.source_path == right.source_path
                            && left.main_repo_path == right.main_repo_path
                            && left.branch == right.branch
                            && left.worktree_path == right.worktree_path
                            && left.managed_by_aoe == right.managed_by_aoe
                            && left.branch_preexisting == right.branch_preexisting
                    })
            }
            (None, None) => true,
            _ => false,
        }
}

pub(crate) fn journal_dir() -> Result<PathBuf> {
    Ok(fs::canonicalize(super::get_app_dir()?)?.join(JOURNAL_DIR_NAME))
}

/// The stable flock is separate from the atomically rewritten JSON inode.
#[derive(Debug)]
pub(crate) struct JournalTransaction {
    pub(crate) path: PathBuf,
    _lease: fs::File,
}

impl JournalTransaction {
    pub(crate) fn rewrite(&self, entry: &MoveJournalEntry) -> Result<()> {
        anyhow::ensure!(
            entry.is_current(),
            "cannot write a historical journal layout"
        );
        atomic_write_verified(&self.path, &serde_json::to_vec_pretty(entry)?)?;
        sync_parent_directory(&self.path)
    }

    pub(crate) fn read(&self) -> Result<MoveJournalEntry> {
        let entry: MoveJournalEntry = serde_json::from_slice(&fs::read(&self.path)?)?;
        anyhow::ensure!(entry.is_current(), "journal requires migration before use");
        Ok(entry)
    }

    pub(crate) fn id(&self) -> &str {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("journal filenames are generated UTF-8 identifiers")
    }

    pub(crate) fn consume(&self) -> Result<()> {
        consume_paths(&self.path)
    }
}

fn open_lease(path: &Path, create: bool) -> Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path.with_extension("lock"))?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "journal lease is not a regular file"
    );
    Ok(file)
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum LeaseState {
    Free,
    Live,
    Missing,
}

/// Probing never creates a sidecar and never proves that Git children stopped.
pub(crate) fn lease_state(path: &Path) -> Result<LeaseState> {
    use fs2::FileExt;
    let file = match open_lease(path, false) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(LeaseState::Missing)
        }
        Err(error) => return Err(error),
    };
    match file.try_lock_exclusive() {
        Ok(()) => Ok(LeaseState::Free),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(LeaseState::Live),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn journal_directories() -> Result<Vec<PathBuf>> {
    let app = super::get_app_dir()?;
    let mut directories = vec![app.join(JOURNAL_DIR_NAME)];
    for profile in super::list_profiles_for_worktree_inventory()? {
        directories.push(app.join("profiles").join(profile).join(JOURNAL_DIR_NAME));
    }
    Ok(directories)
}

pub(crate) fn operator_inventory() -> Result<ScanResult> {
    let mut result = ScanResult {
        entries: Vec::new(),
        unreadable_dirs: Vec::new(),
    };
    for directory in journal_directories()? {
        let scanned = scan_in(&directory);
        result.entries.extend(scanned.entries);
        result.unreadable_dirs.extend(scanned.unreadable_dirs);
    }
    Ok(result)
}

/// Only an attested operator may create a missing lease for old or damaged evidence.
pub(crate) fn resolution_lease(
    root: &super::storage::OwnershipGuard,
    path: &Path,
) -> Result<(JournalTransaction, Vec<u8>)> {
    use fs2::FileExt;
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    root.require_exclusive()?;
    anyhow::ensure!(
        path.extension()
            .is_some_and(|extension| extension == "json"),
        "expected a journal JSON path"
    );
    let parent = fs::canonicalize(
        path.parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or(Path::new(".")),
    )?;
    let app = fs::canonicalize(root.app_dir())?;
    let allowed = journal_directories()?.iter().any(|directory| {
        fs::symlink_metadata(directory)
            .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
            && fs::canonicalize(directory)
                .is_ok_and(|directory| directory == parent && directory.starts_with(&app))
    });
    anyhow::ensure!(
        allowed,
        "target is outside this application's journal directories"
    );
    let path = parent.join(path.file_name().context("journal path has no filename")?);
    let metadata = fs::symlink_metadata(&path)?;
    anyhow::ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "journal is not a regular nofollow file"
    );
    let mut original = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(&path)?;
    anyhow::ensure!(
        original.metadata()?.is_file(),
        "journal is not a regular file"
    );
    let mut bytes = Vec::new();
    original.read_to_end(&mut bytes)?;
    let lease = match open_lease(&path, true) {
        Ok(file) => {
            file.sync_all()?;
            sync_parent_directory(&path)?;
            file
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            open_lease(&path, false)?
        }
        Err(error) => return Err(error),
    };
    lease
        .try_lock_exclusive()
        .context("intent producer is still live; refusing resolution")?;
    anyhow::ensure!(
        super::storage::filesystem_identity(&original.metadata()?)
            == super::storage::filesystem_identity(&fs::symlink_metadata(&path)?)
            && fs::read(&path)? == bytes,
        "journal changed during resolution"
    );
    Ok((
        JournalTransaction {
            path,
            _lease: lease,
        },
        bytes,
    ))
}

pub(crate) fn create_migration_lease(path: &Path) -> Result<()> {
    let file = match open_lease(path, true) {
        Ok(file) => file,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists) =>
        {
            open_lease(path, false)?
        }
        Err(error) => return Err(error),
    };
    file.sync_all()?;
    sync_parent_directory(path)
}

pub(crate) fn binding_path(binding: &str) -> Result<PathBuf> {
    let name = Path::new(binding);
    anyhow::ensure!(
        name.file_name().and_then(|name| name.to_str()) == Some(binding)
            && name
                .extension()
                .is_some_and(|extension| extension == "json"),
        "invalid resource intent binding"
    );
    Ok(journal_dir()?.join(name))
}

pub(crate) fn try_recovery_lease(
    path: &Path,
) -> Result<Option<(JournalTransaction, MoveJournalEntry)>> {
    use fs2::FileExt;
    let file = match open_lease(path, false) {
        Ok(file) => file,
        Err(_) if !path.try_exists()? => return Ok(None),
        Err(error) => return Err(error),
    };
    match file.try_lock_exclusive() {
        Ok(()) => {
            let transaction = JournalTransaction {
                path: path.to_path_buf(),
                _lease: file,
            };
            match fs::read(path) {
                Ok(bytes) => {
                    let entry: MoveJournalEntry = serde_json::from_slice(&bytes)?;
                    anyhow::ensure!(
                        entry.is_current(),
                        "journal requires migration before recovery"
                    );
                    Ok(Some((transaction, entry)))
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(error) => Err(error.into()),
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(error) => Err(error.into()),
    }
}

pub(crate) fn record_live(
    root: &super::storage::OwnershipGuard,
    entry: &MoveJournalEntry,
) -> Result<JournalTransaction> {
    root.require_exclusive()?;
    record_with_sync(entry, &journal_dir()?, sync_parent_directory)
}

/// Model an interrupted producer in recovery fixtures.
#[cfg(test)]
pub(crate) fn record(entry: &MoveJournalEntry) -> Result<PathBuf> {
    Ok(record_with_sync(entry, &journal_dir()?, sync_parent_directory)?.path)
}

fn record_with_sync<S>(
    entry: &MoveJournalEntry,
    dir: &Path,
    mut sync: S,
) -> Result<JournalTransaction>
where
    S: FnMut(&Path) -> Result<()>,
{
    use fs2::FileExt;
    anyhow::ensure!(
        entry.is_current(),
        "new producers require the current journal layout"
    );
    fs::create_dir_all(dir).with_context(|| format!("failed to create {}", dir.display()))?;
    sync(dir)
        .with_context(|| format!("journal directory {} was not made durable", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_nanos();
    let path = dir.join(format!("move-{nanos}-{}.json", uuid::Uuid::new_v4()));
    let lease = open_lease(&path, true)?;
    lease.lock_exclusive()?;
    let transaction = JournalTransaction {
        path,
        _lease: lease,
    };
    atomic_write_verified(&transaction.path, &serde_json::to_vec_pretty(entry)?).with_context(
        || {
            format!(
                "failed to write move journal {}",
                transaction.path.display()
            )
        },
    )?;
    sync(&transaction.path).with_context(|| {
        format!(
            "move journal {} was not made durable",
            transaction.path.display()
        )
    })?;
    Ok(transaction)
}

/// Nanosecond creation order encoded in record's filename. Used only as a
/// durable tie-breaker when two entries share the millisecond JSON timestamp.
pub(crate) fn file_created_at_nanos(path: &Path) -> Option<u128> {
    path.file_stem()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("move-"))
        .and_then(|name| name.split_once('-').map(|(nanos, _)| nanos))
        .and_then(|nanos| nanos.parse().ok())
}

/// Delete one consumed entry and sync its parent directory so the removal
/// itself is durable. Idempotent: a missing file is already consumed.
fn consume_paths(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("failed to remove {}", path.display()))
        }
    }
    // Make JSON removal durable before unlinking its stable lock.
    sync_parent_directory(path)
        .with_context(|| format!("removal of {} was not made durable", path.display()))?;
    match fs::remove_file(path.with_extension("lock")) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    sync_parent_directory(path)
        .with_context(|| format!("removal of {} was not made durable", path.display()))
}

/// Result of scanning the one application-root journal.
pub(crate) struct ScanResult {
    pub(crate) entries: Vec<(PathBuf, std::result::Result<MoveJournalEntry, String>)>,
    pub(crate) unreadable_dirs: Vec<(PathBuf, String)>,
}

pub(crate) fn scan() -> ScanResult {
    let dir = match journal_dir() {
        Ok(dir) => dir,
        Err(error) => {
            return ScanResult {
                entries: Vec::new(),
                unreadable_dirs: vec![(
                    PathBuf::from(JOURNAL_DIR_NAME),
                    format!("resolving application journal: {error:#}"),
                )],
            }
        }
    };
    scan_in(&dir)
}

fn scan_in(dir: &Path) -> ScanResult {
    let mut result = ScanResult {
        entries: Vec::new(),
        unreadable_dirs: Vec::new(),
    };
    if fs::symlink_metadata(dir).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        result.unreadable_dirs.push((
            dir.to_path_buf(),
            "journal directory is an alias, not an original directory".into(),
        ));
        return result;
    }
    {
        let read_dir = match fs::read_dir(dir) {
            Ok(read_dir) => read_dir,
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && matches!(fs::symlink_metadata(dir), Err(missing) if missing.kind() == std::io::ErrorKind::NotFound) =>
            {
                return result
            }
            Err(error) => {
                result.unreadable_dirs.push((
                    dir.to_path_buf(),
                    format!("failed to list {}: {error}", dir.display()),
                ));
                return result;
            }
        };
        let mut paths = Vec::new();
        for entry in read_dir {
            match entry {
                Ok(entry) if entry.path().extension().is_some_and(|ext| ext == "json") => {
                    paths.push(entry.path())
                }
                Ok(_) => {}
                Err(error) => {
                    result.unreadable_dirs.push((
                        dir.to_path_buf(),
                        format!("incomplete journal inventory: {error}"),
                    ));
                    return result;
                }
            }
        }
        paths.sort();
        for path in paths {
            let parsed = fs::read(&path)
                .context("failed to read move journal entry")
                .and_then(|bytes| {
                    serde_json::from_slice::<MoveJournalEntry>(&bytes)
                        .context("malformed move journal entry")
                })
                .map_err(|error| format!("{error:#}"))
                .and_then(|entry| {
                    if entry.is_current() {
                        Ok(entry)
                    } else {
                        Err(format!(
                            "move journal version {} is not supported (current: {MOVE_JOURNAL_VERSION})",
                            entry.version
                        ))
                    }
                });
            result.entries.push((path, parsed));
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(source: &Path, target: &Path) -> MoveJournalEntry {
        MoveJournalEntry {
            version: MOVE_JOURNAL_VERSION,
            ids: vec!["session-id".to_string()],
            source_profile: "source".to_string(),
            target_profile: "target".to_string(),
            source_sessions_path: source.to_path_buf(),
            target_sessions_path: target.to_path_buf(),
            group_move_source_path: "work".to_string(),
            group_move_target_path: "moved".to_string(),
            group_move_subtree: false,
            created_at_epoch_ms: 1,
            authority: MoveAuthority::Metadata {
                source: ProfileAuthority {
                    profile: "source".into(),
                    sessions_path: source.to_path_buf(),
                },
                target: ProfileAuthority {
                    profile: "target".into(),
                    sessions_path: target.to_path_buf(),
                },
                rows: Vec::new(),
            },
        }
    }

    #[test]
    fn record_requires_profile_parent_barrier_before_writing_entry() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source/sessions.json");
        let target = temp.path().join("target/sessions.json");
        fs::create_dir_all(source.parent().unwrap())?;
        fs::create_dir_all(target.parent().unwrap())?;
        let expected_dir = temp.path().join(".move-journal");
        let mut calls = Vec::new();

        let error = record_with_sync(&entry(&source, &target), &expected_dir, |path| {
            calls.push(path.to_path_buf());
            Err(anyhow::anyhow!("forced profile-parent barrier failure"))
        })
        .expect_err("journal record must fail before an unverified directory can be used");

        assert!(error.to_string().contains("journal directory"));
        assert_eq!(calls, vec![expected_dir.clone()]);
        assert!(
            expected_dir.exists(),
            "directory was created before its barrier"
        );
        assert!(
            fs::read_dir(expected_dir)?.next().is_none(),
            "no entry may be written before the parent barrier"
        );
        Ok(())
    }

    #[test]
    fn scan_separates_directory_failures_from_entry_results() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let sessions = temp.path().join("profile/sessions.json");
        fs::create_dir_all(sessions.parent().unwrap())?;
        let journal_dir = temp.path().join(".move-journal");
        fs::write(&journal_dir, b"not-a-directory")?;

        let result = scan_in(&journal_dir);

        assert!(result.entries.is_empty());
        assert_eq!(result.unreadable_dirs.len(), 1);
        assert_eq!(result.unreadable_dirs[0].0, journal_dir);
        Ok(())
    }
}
