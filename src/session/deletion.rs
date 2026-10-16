//! Shared session deletion logic used by CLI, TUI, and web server.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;

use super::claim::ResourceReservation;
use super::move_journal::{
    EffectObservation, IntentOwner, ProfileAuthority, ResourceEffect, ResourceIntent, ResourceKey,
};
use crate::containers::DockerContainer;
use crate::git::cleanup::remove_managed_worktree;
use crate::git::GitWorktree;
use crate::session::config::repo_config;
use crate::session::storage::{acquire_ownership_lock, OwnershipGuard};
use crate::session::{Instance, LifecycleOperation, Storage};

#[derive(Clone)]
pub struct DeletionRequest {
    pub session_id: String,
    pub instance: Instance,
    pub delete_worktree: bool,
    pub delete_branch: bool,
    pub delete_sandbox: bool,
    pub force_delete: bool,
    /// When `true`, on_destroy hooks run detached from the controlling terminal (TUI/web).
    pub detach_hooks: bool,
    /// When `true` AND `instance.scratch` is `true`, the scratch directory is left on disk instead
    /// of being removed.
    pub keep_scratch: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeletionDisposition {
    Removed,
    KeptRestored,
    AlreadyGone,
    Busy,
    Failed,
}

#[derive(Debug)]
pub struct DeletionResult {
    pub session_id: String,
    pub success: bool,
    pub messages: Vec<String>,
    pub errors: Vec<String>,
    pub disposition: DeletionDisposition,
    pub teardown_started: bool,
    /// Latest durable row when the transaction deliberately kept it.
    pub retained_instance: Option<Instance>,
}

impl DeletionResult {
    fn rejected(
        session_id: String,
        disposition: DeletionDisposition,
        message: impl Into<String>,
        retained_instance: Option<Instance>,
    ) -> Self {
        Self {
            session_id,
            success: false,
            messages: Vec::new(),
            errors: vec![message.into()],
            disposition,
            retained_instance,
            teardown_started: false,
        }
    }
}

pub enum PurgeReservation {
    Reserved(PurgeTransaction),
    Rejected(DeletionResult),
}

/// A durable resource reservation; no metadata flock survives an external wait.
pub struct PurgeTransaction {
    storage: Storage,
    request: DeletionRequest,
    transaction: Arc<ResourceReservation>,
    effect_count: usize,
    messages: Vec<String>,
    hook_error: Option<String>,
    hooks_ran: bool,
    active: bool,
}

#[must_use = "committed purge sidecars must be finished"]
pub struct CommittedPurge {
    transaction: PurgeTransaction,
}

impl PurgeTransaction {
    pub fn reserve_unwatched(request: DeletionRequest) -> Result<PurgeReservation> {
        anyhow::ensure!(
            !request.instance.source_profile.is_empty(),
            "session has no source profile; refusing to use the default profile"
        );
        let storage = Storage::open_unwatched(&request.instance.source_profile)?;
        Self::reserve(storage, request)
    }

    pub fn reserve(storage: Storage, mut request: DeletionRequest) -> Result<PurgeReservation> {
        let (before, reserved, paths) = {
            let root = acquire_ownership_lock()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &request.session_id)?;
            let mut rows = storage.load_strict_for_worktree_ownership()?;
            let before = rows
                .iter()
                .find(|row| row.id == request.session_id)
                .cloned();
            let decision = super::claim::decide_purge_claim(
                &mut rows,
                &request.session_id,
                request.instance.is_trashed(),
                Utc::now(),
            )?;
            let disposition = match decision {
                super::claim::PurgeClaimDecision::Claimed(_) => None,
                super::claim::PurgeClaimDecision::Restored => Some((
                    DeletionDisposition::KeptRestored,
                    "Session was restored, so it was not purged".to_string(),
                )),
                super::claim::PurgeClaimDecision::Busy(holder) => Some((
                    DeletionDisposition::Busy,
                    format!("Session {}", holder.already_in_progress_reason()),
                )),
                super::claim::PurgeClaimDecision::AlreadyGone => Some((
                    DeletionDisposition::AlreadyGone,
                    "Session was already removed by another process".to_string(),
                )),
            };
            if let Some((disposition, message)) = disposition {
                return Ok(PurgeReservation::Rejected(DeletionResult::rejected(
                    request.session_id.clone(),
                    disposition,
                    message,
                    before,
                )));
            }
            let before = before.context("purge owner disappeared")?;
            anyhow::ensure!(
                before.created_at == request.instance.created_at,
                "purge incarnation was superseded"
            );
            let reserved = rows
                .into_iter()
                .find(|row| row.id == request.session_id)
                .context("purge reservation produced no row")?;
            let paths = paths_in_use_except_with_ownership(
                &root,
                storage.profile(),
                &[&request.session_id],
            );
            if let PathsInUse::Unknown(reason) = &paths {
                anyhow::bail!("other sessions could not be checked: {reason}");
            }
            (before, reserved, paths)
        };
        if request.instance.is_trashed() && before.trashed_at != request.instance.trashed_at {
            request.force_delete = false;
        }
        request.instance = before.clone();
        request.instance.source_profile = storage.profile().to_string();
        let mut messages = Vec::new();
        let (effects, resources) = plan_purge(&request, &paths, &mut messages)?;
        let effect_count = effects.len();
        let root = acquire_ownership_lock()?;
        let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
        let _lifecycle =
            storage.acquire_instance_lifecycle_lock_with_ownership(&root, &request.session_id)?;
        let authority = ProfileAuthority::capture(&storage)?;
        let intent = ResourceIntent::new(
            IntentOwner {
                profile: authority.clone(),
                id: before.id.clone(),
                created_at: before.created_at,
                generation: reserved.lifecycle_generation,
            },
            authority,
            LifecycleOperation::Purge,
            Some(before),
            None,
            effects,
            resources,
        )?;
        let admitted =
            ResourceReservation::admit_with_ownership(&storage, &root, intent, reserved)?;
        request.instance = admitted.instance;
        Ok(PurgeReservation::Reserved(Self {
            storage,
            request,
            transaction: admitted.transaction,
            effect_count,
            messages,
            hook_error: None,
            hooks_ran: false,
            active: true,
        }))
    }

    pub fn run_hooks(self) -> Self {
        self.run_hooks_with(run_on_destroy_hooks)
    }

    fn run_hooks_with(mut self, hooks: impl FnOnce(&Instance, bool)) -> Self {
        self.hooks_ran = true;
        if let Err(error) = self
            .transaction
            .run_effect(0, || {
                hooks(&self.request.instance, self.request.detach_hooks);
                Ok(((), None))
            })
            .and_then(|result| result)
        {
            self.hook_error = Some(format!("Purge hook phase retained its intent: {error:#}"));
        }
        self
    }

    pub fn begin_irreversible(
        mut self,
    ) -> std::result::Result<CommittedPurge, Box<DeletionResult>> {
        if !self.hooks_ran {
            self = self.run_hooks();
        }
        let prepare = (|| -> Result<_> {
            anyhow::ensure!(
                self.hook_error.is_none(),
                "{}",
                self.hook_error.as_deref().unwrap_or_default()
            );
            let quiesce = self.transaction.effect(1)?;
            self.transaction.run_effect(1, || {
                let (messages, observation) = execute_purge_effect(
                    &self.request.instance,
                    &quiesce,
                    self.request.delete_sandbox,
                    |_| anyhow::bail!("quiesce cannot release a container"),
                )?;
                self.messages.extend(messages);
                Ok(((), observation))
            })??;
            let paths = {
                let root = crate::session::storage::acquire_ownership_read()?;
                let paths = paths_in_use_except_with_ownership(
                    &root,
                    self.storage.profile(),
                    &[&self.request.session_id],
                );
                if let PathsInUse::Unknown(reason) = &paths {
                    anyhow::bail!("other sessions could not be checked: {reason}");
                }
                paths
            };
            let mut errors = Vec::new();
            let mut messages = Vec::new();
            let preserved = stage_collect_preserved_worktrees(
                &self.request,
                purge_repos(&self.request),
                &paths,
                &mut errors,
                &mut messages,
            );
            anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
            for index in 1..self.effect_count {
                match self.transaction.effect(index)? {
                    ResourceEffect::RemoveWorktree { path, .. } => anyhow::ensure!(
                        !preserved.contains(&path),
                        "purge worktree became protected: {}",
                        path.display(),
                    ),
                    ResourceEffect::RemoveDirectory { path } => anyhow::ensure!(
                        !paths.covers(&path) && super::scratch::is_scratch_path(&path),
                        "scratch directory became claimed or is not AoE-owned",
                    ),
                    _ => {}
                }
            }
            let root = acquire_ownership_lock()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = self
                .storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &self.request.session_id)?;
            self.request.instance = self.transaction.retire_for_purge_with_ownership(&root)?;
            Ok(())
        })();
        match prepare {
            Ok(()) => {
                self.active = false;
                Ok(CommittedPurge { transaction: self })
            }
            Err(error) => {
                let rollback = self.transaction.rollback();
                self.active = false;
                let mut message = format!("Purge refused before teardown: {error:#}");
                if let Err(rollback) = rollback {
                    message.push_str(&format!("; recovery intent retained: {rollback:#}"));
                }
                Err(Box::new(DeletionResult::rejected(
                    self.request.session_id.clone(),
                    DeletionDisposition::Failed,
                    message,
                    self.storage
                        .load_strict_for_worktree_ownership()
                        .ok()
                        .and_then(|rows| {
                            rows.into_iter()
                                .find(|row| row.id == self.request.session_id)
                        }),
                )))
            }
        }
    }

    pub fn complete(self) -> DeletionResult {
        match self.begin_irreversible() {
            Ok(committed) => committed.finish(),
            Err(result) => *result,
        }
    }
}

impl CommittedPurge {
    pub fn finish(self) -> DeletionResult {
        self.finish_with(|id| DockerContainer::from_session_id(id).teardown(id))
    }

    fn finish_with(
        self,
        teardown: impl FnOnce(&str) -> crate::containers::Teardown,
    ) -> DeletionResult {
        let Self {
            transaction: mut purge,
        } = self;
        #[cfg(test)]
        if let Some(hook) = BEFORE_COMMITTED_PURGE_RESUME.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let mut result = DeletionResult {
            session_id: purge.request.session_id.clone(),
            success: true,
            messages: std::mem::take(&mut purge.messages),
            errors: Vec::new(),
            disposition: DeletionDisposition::Removed,
            teardown_started: true,
            retained_instance: None,
        };
        let mut teardown = Some(teardown);
        for index in 2..purge.effect_count {
            let step = (|| -> Result<()> {
                let effect = purge.transaction.effect(index)?;
                purge.transaction.run_effect(index, || {
                    let (messages, observation) = execute_purge_effect(
                        &purge.request.instance,
                        &effect,
                        purge.request.delete_sandbox,
                        |id| {
                            teardown
                                .take()
                                .context("container teardown already ran")
                                .map(|teardown| teardown(id))
                        },
                    )?;
                    result.messages.extend(messages);
                    Ok(((), observation))
                })??;
                Ok(())
            })();
            if let Err(error) = step {
                result.success = false;
                result.errors.push(format!(
                    "Purge cleanup retained its recovery intent: {error:#}"
                ));
                return result;
            }
        }
        let publication = (|| -> Result<()> {
            let root = acquire_ownership_lock()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = purge
                .storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &purge.request.session_id)?;
            purge
                .transaction
                .publish_with_ownership(&root, |_, _| Ok(()))?;
            Ok(())
        })();
        if let Err(error) = publication {
            result.success = false;
            result.errors.push(format!(
                "Purge completed but its recovery intent remains: {error:#}"
            ));
        }
        result
    }
}

/// The producer and rowless recovery execute exactly the same durable cleanup.
pub(crate) fn execute_purge_effect(
    instance: &Instance,
    effect: &ResourceEffect,
    delete_sandbox: bool,
    teardown: impl FnOnce(&str) -> Result<crate::containers::Teardown>,
) -> Result<(Vec<String>, Option<EffectObservation>)> {
    let mut messages = Vec::new();
    let mut observation = None;
    match effect {
        ResourceEffect::ReleaseContainer => {
            let mut errors = Vec::new();
            deletion_messages_for(teardown(&instance.id)?, &mut messages, &mut errors);
            anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
            observation = Some(EffectObservation {
                container_released: Some(true),
                ..EffectObservation::default()
            });
        }
        ResourceEffect::RemoveWorktree {
            repository,
            path,
            force,
        } => {
            remove_managed_worktree(
                &GitWorktree::new(repository.clone())?,
                path,
                repository,
                instance,
                *force,
                delete_sandbox,
            )
            .map_err(|errors| anyhow::anyhow!(errors.join("; ")))?;
            messages.push(format!("Worktree removed: {}", path.display()));
        }
        ResourceEffect::PurgeAcpEvents { database } => {
            let unused = {
                let root = acquire_ownership_lock()?;
                committed_identity_is_unused(&root, instance)?
            };
            if !unused {
                messages.push(
                    "ACP worker and transcript kept: another profile uses the session ID".into(),
                );
                return Ok((messages, None));
            }
            crate::process::worker_registry::terminate(&instance.id)?;
            crate::acp::event_store::purge_acp_transcript_rows(database, &instance.id)?;
        }
        ResourceEffect::PurgeSidecars { stores } => {
            let unused = {
                let root = acquire_ownership_lock()?;
                committed_identity_is_unused(&root, instance)?
            };
            if !unused {
                messages.push(
                    "Agent stores and hook status kept: another profile uses the session ID".into(),
                );
                return Ok((messages, None));
            }
            if let Some(stores) = stores {
                let (paths, _) = stores.remove(&instance.id)?;
                for path in paths {
                    messages.push(format!("Agent store removed: {}", path.display()));
                }
            }
            crate::hooks::cleanup_hook_status_dir(&instance.id);
        }
        ResourceEffect::Callback {
            name, directory, ..
        } => match name.as_str() {
            "purge-quiesce" => instance.kill_all_tmux_sessions_locked(),
            "purge-preclean" => {
                let _ = crate::git::cleanup::cleanup_sandbox_worktree(instance);
            }
            "purge-empty-workspace" => match std::fs::remove_dir(directory) {
                Ok(()) => messages.push("Workspace directory removed".into()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
                    messages.push("Workspace directory kept: it is not empty".into())
                }
                Err(error) => return Err(error.into()),
            },
            _ => anyhow::bail!("purge callback cannot be replayed: {name}"),
        },
        _ => messages.extend(effect.execute()?),
    }
    Ok((messages, observation))
}

pub(crate) fn retryable_purge_cleanup(effect: &ResourceEffect) -> bool {
    match effect {
        ResourceEffect::ReleaseContainer
        | ResourceEffect::RemoveWorktree { .. }
        | ResourceEffect::RemoveDirectory { .. }
        | ResourceEffect::PurgeAcpEvents { .. }
        | ResourceEffect::PurgeSidecars { .. } => true,
        ResourceEffect::Callback { name, .. } => matches!(
            name.as_str(),
            "purge-quiesce" | "purge-preclean" | "purge-empty-workspace"
        ),
        _ => false,
    }
}

impl Drop for PurgeTransaction {
    fn drop(&mut self) {
        if self.active {
            let transaction = Arc::clone(&self.transaction);
            if let Err(error) = std::thread::Builder::new().name("aoe-purge-cancel".into())
                .spawn(move || {
                    if let Err(error) = transaction.rollback() {
                        tracing::warn!(target: "session.delete", "cancelled purge retained its intent: {error:#}");
                    }
                })
            {
                tracing::warn!(target: "session.delete", "could not schedule purge cancellation: {error}");
            }
        }
    }
}

fn purge_repos(request: &DeletionRequest) -> &[super::WorkspaceRepo] {
    if request
        .instance
        .workspace_info
        .as_ref()
        .is_some_and(|info| info.cleanup_on_delete)
    {
        request.instance.all_repos()
    } else {
        &[]
    }
}

fn is_purge_sandboxed(request: &DeletionRequest) -> bool {
    request
        .instance
        .sandbox_info
        .as_ref()
        .is_some_and(|info| info.enabled)
}

fn plan_purge(
    request: &DeletionRequest,
    paths: &PathsInUse,
    messages: &mut Vec<String>,
) -> Result<(Vec<ResourceEffect>, Vec<ResourceKey>)> {
    let repos = purge_repos(request);
    let mut errors = Vec::new();
    let preserved = stage_collect_preserved_worktrees(request, repos, paths, &mut errors, messages);
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
    let callback = |name: &str, directory: PathBuf| ResourceEffect::Callback {
        name: name.into(),
        directory,
        checkouts: Vec::new(),
    };
    let cwd = PathBuf::from(&request.instance.project_path);
    let mut effects = vec![
        callback("purge-hooks", cwd.clone()),
        callback("purge-quiesce", cwd.clone()),
        ResourceEffect::PurgeAcpEvents {
            database: crate::session::get_app_dir()?.join("acp_events.db"),
        },
    ];
    let mut removals = Vec::new();
    let mut branches = Vec::new();
    let primary = request.instance.worktree_info.as_ref().map(|info| {
        (
            Path::new(&info.main_repo_path),
            cwd.as_path(),
            info.branch.as_str(),
            info.managed_by_aoe,
            info.managed_by_aoe,
        )
    });
    let candidates = primary.into_iter().chain(repos.iter().map(|repo| {
        (
            Path::new(&repo.main_repo_path),
            Path::new(&repo.worktree_path),
            repo.branch.as_str(),
            repo.managed_by_aoe,
            !repo.branch_preexisting,
        )
    }));
    for (repository, path, branch, managed, owns_branch) in candidates {
        if !request.delete_worktree || !managed || preserved.contains(path) {
            continue;
        }
        removals.push(ResourceEffect::RemoveWorktree {
            repository: repository.to_path_buf(),
            path: path.to_path_buf(),
            force: request.force_delete,
        });
        if request.delete_branch && owns_branch {
            let git = GitWorktree::new(repository.to_path_buf())?;
            let registered = git.worktree_path_for_branch(branch)?;
            let bound = registered.as_deref().and_then(resolve_claim_path)
                == resolve_claim_path(path)
                && registered.is_some();
            let shared = {
                let root = crate::session::storage::acquire_ownership_read()?;
                branch_in_use_with_ownership(
                    &root,
                    &request.instance.source_profile,
                    &[&request.session_id],
                    repository,
                    branch,
                )?
            };
            if bound && !shared {
                branches.push(ResourceEffect::DeleteBranch {
                    repository: repository.to_path_buf(),
                    branch: branch.into(),
                    checkouts: vec![path.to_path_buf()],
                });
            } else {
                messages.push(format!(
                    "Branch '{branch}' kept; no exclusive registered checkout ownership"
                ));
            }
        }
    }
    if request.delete_worktree
        && is_purge_sandboxed(request)
        && !removals.is_empty()
        && preserved.is_empty()
        && repos.iter().all(|repo| repo.managed_by_aoe)
    {
        effects.push(callback("purge-preclean", cwd.clone()));
    }
    if request.delete_sandbox && is_purge_sandboxed(request) {
        effects.push(ResourceEffect::ReleaseContainer);
    }
    effects.extend(removals);
    effects.extend(branches);
    let mut resources = Vec::new();
    if request.delete_worktree {
        if let Some(info) = &request.instance.workspace_info {
            let path = PathBuf::from(&info.workspace_dir);
            if info.cleanup_on_delete && preserved.is_empty() && !paths.covers(&path) {
                anyhow::ensure!(
                    workspace_dir_is_aoe_owned(info),
                    "workspace directory is not AoE-owned"
                );
                resources.push(ResourceKey::path(&path, true)?);
                effects.push(callback("purge-empty-workspace", path));
            }
        }
    }
    if request.instance.scratch
        && !request.keep_scratch
        && !paths.covers(&cwd)
        && cwd.try_exists()?
    {
        anyhow::ensure!(
            super::scratch::is_scratch_path(&cwd),
            "scratch directory is not AoE-owned"
        );
        effects.push(ResourceEffect::RemoveDirectory { path: cwd.clone() });
    }
    let stores = if request.delete_sandbox && is_purge_sandboxed(request) {
        Some(super::sandbox_store_reclaim::StoreRemoval::capture(
            &request.instance,
        )?)
    } else {
        None
    };
    effects.push(ResourceEffect::PurgeSidecars { stores });
    Ok((effects, resources))
}

pub fn execute_deletion(request: DeletionRequest) -> DeletionResult {
    let id = request.session_id.clone();
    let recent_entry = crate::session::recent_project_entry_for(&request.instance);
    let result = match PurgeTransaction::reserve_unwatched(request) {
        Ok(PurgeReservation::Reserved(transaction)) => transaction.run_hooks().complete(),
        Ok(PurgeReservation::Rejected(result)) => result,
        Err(error) => DeletionResult::rejected(
            id,
            DeletionDisposition::Failed,
            format!("Could not reserve session deletion: {error}"),
            None,
        ),
    };
    if result.disposition == DeletionDisposition::Removed {
        if let Some(entry) = recent_entry {
            if let Err(error) = crate::session::record_recent_project(entry) {
                tracing::warn!(
                    target: "session.delete",
                    "recording recent project after delete failed: {error}"
                );
            }
        }
    }
    result
}

/// Whether `workspace_dir` has the workspace layout AoE creates and may therefore be removed once
/// empty.
fn workspace_dir_is_aoe_owned(ws_info: &crate::session::WorkspaceInfo) -> bool {
    let ws_path = Path::new(&ws_info.workspace_dir);
    if ws_info.repos.is_empty() {
        return false;
    }
    ws_info.repos.iter().all(|repo| {
        let worktree = Path::new(&repo.worktree_path);
        worktree != ws_path && worktree.starts_with(ws_path)
    })
}

/// Whether `branch` is one of the branches git states is `main_repo`'s default, so its worktree
/// must be preserved.
fn is_protected_default_branch(main_repo: &Path, branch: &str) -> bool {
    GitWorktree::new(main_repo.to_path_buf())
        .and_then(|git| git.protected_default_branch_names())
        .is_ok_and(|names| names.contains(branch))
}

/// Generic cwd claims protect their ancestors; explicit checkouts also protect descendants.
pub(crate) struct PathClaim {
    path: PathBuf,
    recursive: bool,
}

fn other_sessions_paths(instances: &[Instance], except_ids: &[&str]) -> Vec<PathClaim> {
    let mut paths = Vec::new();
    for instance in instances
        .iter()
        .filter(|row| !except_ids.contains(&row.id.as_str()))
    {
        let mut add = |path: &str, recursive| {
            paths.push(PathClaim {
                path: PathBuf::from(path),
                recursive,
            })
        };
        add(&instance.project_path, instance.worktree_info.is_some());
        if let Some(path) = &instance.pre_trash_project_path {
            add(path, instance.worktree_info.is_some());
        }
        if let Some(workspace) = &instance.workspace_info {
            add(&workspace.workspace_dir, true);
            for repo in &workspace.repos {
                add(&repo.worktree_path, true);
            }
        }
    }
    paths
}
/// Resolve absent tails only through components proven missing, never broken aliases.
pub(crate) fn resolve_claim_path(path: &Path) -> Option<PathBuf> {
    if path.as_os_str().is_empty() {
        return None;
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().ok()?.join(path)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            std::path::Component::RootDir => resolved.push(component.as_os_str()),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::Normal(name) => {
                resolved.push(name);
                match std::fs::symlink_metadata(&resolved) {
                    Ok(_) => resolved = resolved.canonicalize().ok()?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return None,
                }
            }
        }
    }
    Some(resolved)
}

pub(crate) fn claim_path_blocks(left: &Path, right: &Path, recursive: bool) -> bool {
    match (resolve_claim_path(left), resolve_claim_path(right)) {
        (Some(left), Some(right)) => {
            left.starts_with(&right) || (recursive && right.starts_with(&left))
        }
        _ => true,
    }
}

/// The paths sessions outside a deletion use, across every profile.
pub(crate) enum PathsInUse {
    Known(Vec<PathClaim>),
    /// Some store could not be read, so every path must be assumed in use.
    Unknown(String),
}

impl PathsInUse {
    pub(crate) fn covers(&self, root: &Path) -> bool {
        match self {
            Self::Known(paths) => {
                resolve_claim_path(root).is_none()
                    || paths
                        .iter()
                        .any(|path| claim_path_blocks(&path.path, root, path.recursive))
            }
            Self::Unknown(_) => true,
        }
    }

    fn reason(&self) -> String {
        match self {
            Self::Known(_) => "another session still uses it".to_string(),
            Self::Unknown(reason) => format!("other sessions could not be checked ({reason})"),
        }
    }
}

fn committed_identity_is_unused(ownership: &OwnershipGuard, owner: &Instance) -> Result<bool> {
    ownership.require_exclusive()?;
    let (_, storages) =
        all_profile_storages_with_ownership(ownership).map_err(anyhow::Error::msg)?;
    let mut unused = true;
    for storage in &storages {
        for row in storage.load_strict_for_worktree_ownership()? {
            if row.id == owner.id {
                anyhow::ensure!(
                    storage.profile() != owner.source_profile && row.created_at == owner.created_at,
                    "purged session identity was reused"
                );
                unused = false;
            }
        }
    }
    Ok(unused)
}

fn all_profile_storages_with_ownership(
    ownership: &OwnershipGuard,
) -> std::result::Result<(Vec<String>, Vec<Storage>), String> {
    let profiles = crate::session::list_profiles_for_worktree_inventory()
        .map_err(|error| format!("listing profiles: {error}"))?;
    let mut storages = Vec::new();
    let mut identities = std::collections::HashSet::new();
    for profile in &profiles {
        let storage = Storage::open_unwatched_with_ownership(profile, ownership)
            .map_err(|error| format!("opening profile {profile}: {error}"))?;
        let identity = resolve_claim_path(storage.sessions_path())
            .ok_or_else(|| format!("resolving profile {profile}: invalid sessions path"))?;
        if identities.insert(identity) {
            storages.push(storage);
        }
    }
    Ok((profiles, storages))
}

/// Check the one durable intent inventory and every profile under the namespace fence.
pub(crate) fn ensure_resource_intent_available(
    ownership: &OwnershipGuard,
    journal: &super::move_journal::JournalTransaction,
    intent: &super::move_journal::ResourceIntent,
) -> Result<()> {
    use super::move_journal::{EffectPhase, ProfileAuthority, ResourceKey};
    ownership.require_exclusive()?;
    for (path, entry) in super::move_journal::resource_inventory()? {
        if path == journal.path {
            continue;
        }
        anyhow::ensure!(
            !entry.ids.iter().any(|id| id == &intent.owner.id)
                || (!entry.affects_profile(&intent.owner.profile)
                    && !entry.affects_profile(&intent.target)),
            "session identity is busy in {}",
            path.display()
        );
        anyhow::ensure!(
            !intent.resources.iter().any(|key| {
                entry
                    .reserved_resources()
                    .iter()
                    .any(|held| key.intent_overlaps(held))
            }),
            "checkout resource is busy in {}",
            path.display()
        );
    }
    if intent.resources.is_empty() {
        return Ok(());
    }
    let (_, storages) =
        all_profile_storages_with_ownership(ownership).map_err(anyhow::Error::msg)?;
    let branches = intent
        .resources
        .iter()
        .any(|key| matches!(key, ResourceKey::Branch { .. }));
    super::storage::with_storages_locked_with_ownership(ownership, &storages, || -> Result<()> {
        for storage in &storages {
            let profile = ProfileAuthority::capture(storage)?;
            for row in storage.load_strict_for_worktree_ownership()? {
                let source = profile.sessions_path == intent.owner.profile.sessions_path;
                let initial = source
                    && intent.phase == EffectPhase::Prepared
                    && intent.before.as_ref().is_some_and(|before| {
                        super::move_journal::row_resources_match(&row, before)
                            && row.lifecycle_reservation == before.lifecycle_reservation
                    });
                let destination = if intent.rollback {
                    &intent.owner.profile
                } else {
                    &intent.target
                };
                let published = profile.sessions_path == destination.sessions_path
                    && row.id == intent.owner.id
                    && row.created_at == intent.owner.created_at
                    && row.lifecycle_generation == intent.owner.generation
                    && row.lifecycle_reservation.is_none()
                    && (if intent.rollback {
                        intent.before.as_ref()
                    } else {
                        intent.after.as_ref()
                    })
                    .is_some_and(|goal| super::move_journal::same_namespace(&row, goal));
                if initial || published || intent.row_is_reserved(&profile, &row, journal.id()) {
                    continue;
                }
                anyhow::ensure!(
                    !source || row.id != intent.owner.id,
                    "resource lifecycle owner was superseded"
                );
                let keys = if branches {
                    super::move_journal::row_resource_keys(&row)?
                } else {
                    super::move_journal::row_path_keys(&row)?
                };
                anyhow::ensure!(
                    !intent
                        .resources
                        .iter()
                        .any(|held| keys.iter().any(|key| held.overlaps(key))),
                    "checkout resource is already claimed by session {} in profile {}",
                    row.id,
                    storage.profile()
                );
            }
        }
        Ok(())
    })?
}

fn scan_paths_in_use_qualified(
    storages: &[Storage],
    profile: &str,
    except_ids: &[&str],
) -> PathsInUse {
    let owner = if except_ids.is_empty() {
        None
    } else {
        // Resolve aliases to the same logical profile route.
        let identity = match crate::session::get_profile_dir_path(profile)
            .and_then(|directory| Ok(directory.canonicalize()?))
        {
            Ok(directory) if !profile.is_empty() => directory,
            _ => return PathsInUse::Unknown("owner profile cannot be resolved".into()),
        };
        Some(identity)
    };
    let mut owner_seen = owner.is_none();
    let mut paths = Vec::new();
    for storage in storages {
        let identity = match storage
            .sessions_path()
            .parent()
            .ok_or_else(|| anyhow::anyhow!("profile sessions path has no parent"))
            .and_then(|directory| Ok(directory.canonicalize()?))
        {
            Ok(identity) => identity,
            Err(error) => return PathsInUse::Unknown(format!("resolving profile: {error}")),
        };
        let excluded = owner.as_ref() == Some(&identity);
        owner_seen |= excluded;
        let instances = match storage.load_strict_for_worktree_ownership() {
            Ok(instances) => instances,
            Err(error) => {
                return PathsInUse::Unknown(format!(
                    "reading profile {}: {error}",
                    storage.profile()
                ))
            }
        };
        for path in other_sessions_paths(&instances, if excluded { except_ids } else { &[] }) {
            let Some(resolved) = resolve_claim_path(&path.path) else {
                return PathsInUse::Unknown("a session ownership path cannot be resolved".into());
            };
            paths.push(PathClaim {
                path: resolved,
                recursive: path.recursive,
            });
        }
    }
    if !owner_seen {
        return PathsInUse::Unknown("owner profile is missing from ownership inventory".into());
    }
    PathsInUse::Known(paths)
}

pub(crate) fn paths_in_use_except_with_ownership(
    ownership: &OwnershipGuard,
    profile: &str,
    except_ids: &[&str],
) -> PathsInUse {
    let (_, storages) = match all_profile_storages_with_ownership(ownership) {
        Ok(found) => found,
        Err(reason) => return PathsInUse::Unknown(reason),
    };
    crate::session::storage::with_storages_locked_with_ownership(ownership, &storages, || {
        scan_paths_in_use_qualified(&storages, profile, except_ids)
    })
    .unwrap_or_else(|error| PathsInUse::Unknown(format!("locking session stores: {error}")))
}

pub(crate) fn branch_in_use_with_ownership(
    ownership: &OwnershipGuard,
    profile: &str,
    except_ids: &[&str],
    main_repo: &Path,
    branch: &str,
) -> Result<bool> {
    let (_, storages) =
        all_profile_storages_with_ownership(ownership).map_err(anyhow::Error::msg)?;
    let main_repo = resolve_claim_path(main_repo)
        .ok_or_else(|| anyhow::anyhow!("repository ownership path cannot be resolved"))?;
    let owner_identity = crate::session::get_profile_dir_path(profile)?.canonicalize()?;
    crate::session::storage::with_storages_locked_with_ownership(
        ownership,
        &storages,
        || -> Result<bool> {
            for storage in &storages {
                for row in storage.load_strict_for_worktree_ownership()? {
                    if storage
                        .sessions_path()
                        .parent()
                        .context("profile path has no parent")?
                        .canonicalize()?
                        == owner_identity
                        && except_ids.contains(&row.id.as_str())
                    {
                        continue;
                    }
                    if let Some(wt) = &row.worktree_info {
                        if wt.branch == branch
                            && resolve_claim_path(Path::new(&wt.main_repo_path)).ok_or_else(
                                || anyhow::anyhow!("repository ownership path cannot be resolved"),
                            )? == main_repo
                        {
                            return Ok(true);
                        }
                    }
                    for repo in row.all_repos() {
                        if repo.branch == branch
                            && resolve_claim_path(Path::new(&repo.main_repo_path)).ok_or_else(
                                || anyhow::anyhow!("repository ownership path cannot be resolved"),
                            )? == main_repo
                        {
                            return Ok(true);
                        }
                    }
                }
            }
            Ok(false)
        },
    )?
}

#[cfg(test)]
thread_local! {
    static BEFORE_COMMITTED_PURGE_RESUME: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub fn perform_deletion(request: &DeletionRequest) -> DeletionResult {
    perform_deletion_with(request, |id| {
        DockerContainer::from_session_id(id).teardown(id)
    })
}

#[cfg(test)]
fn perform_deletion_with(
    request: &DeletionRequest,
    teardown: impl FnOnce(&str) -> crate::containers::Teardown,
) -> DeletionResult {
    let mut request = request.clone();
    if request.instance.source_profile.is_empty() {
        request.instance.source_profile = "default".into();
    }
    let execute = (|| -> Result<DeletionResult> {
        let storage = Storage::open_unwatched(&request.instance.source_profile)?;
        storage.update(|rows, _| {
            if !rows.iter().any(|row| row.id == request.session_id) {
                rows.push(request.instance.clone());
            }
            Ok(())
        })?;
        match PurgeTransaction::reserve(storage, request.clone())? {
            PurgeReservation::Rejected(result) => Ok(result),
            PurgeReservation::Reserved(transaction) => {
                match transaction.run_hooks().begin_irreversible() {
                    Ok(committed) => Ok(committed.finish_with(teardown)),
                    Err(result) => Ok(*result),
                }
            }
        }
    })();
    execute.unwrap_or_else(|error| {
        DeletionResult::rejected(
            request.session_id,
            DeletionDisposition::Failed,
            error.to_string(),
            None,
        )
    })
}

fn stage_collect_preserved_worktrees(
    request: &DeletionRequest,
    repos: &[super::WorkspaceRepo],
    paths_in_use: &PathsInUse,
    errors: &mut Vec<String>,
    messages: &mut Vec<String>,
) -> std::collections::HashSet<PathBuf> {
    let mut preserved_worktree_paths: std::collections::HashSet<PathBuf> =
        std::collections::HashSet::new();

    // Default-branch guard, deliberately NOT behind the `!force_delete` gate below.
    if request.delete_worktree {
        if let Some(wt_info) = &request.instance.worktree_info {
            if wt_info.managed_by_aoe
                && is_protected_default_branch(Path::new(&wt_info.main_repo_path), &wt_info.branch)
            {
                let path = PathBuf::from(&request.instance.project_path);
                tracing::warn!(target: "session.delete",
                    session_id = %request.session_id,
                    branch = %wt_info.branch,
                    path = %path.display(),
                    "perform_deletion: preserving the worktree of a default branch"
                );
                messages.push(format!(
                    "Worktree preserved; '{}' is a default branch of its repository",
                    wt_info.branch
                ));
                preserved_worktree_paths.insert(path);
            }
        }
        for repo in repos.iter().filter(|r| r.managed_by_aoe) {
            if is_protected_default_branch(Path::new(&repo.main_repo_path), &repo.branch) {
                tracing::warn!(target: "session.delete",
                    session_id = %request.session_id,
                    repo = %repo.name,
                    branch = %repo.branch,
                    path = %repo.worktree_path,
                    "perform_deletion: preserving the worktree of a default branch"
                );
                messages.push(format!(
                    "Workspace ({}) worktree preserved; '{}' is a default branch of its repository",
                    repo.name, repo.branch
                ));
                preserved_worktree_paths.insert(PathBuf::from(&repo.worktree_path));
            }
        }
    }

    // A worktree another session still works in, or may, is kept, and so its branch, whichever
    // sessions the caller named. Checked before the dirty gate so a kept worktree cannot fail the
    // deletion.
    if request.delete_worktree {
        let in_use = |root: &Path| paths_in_use.covers(root);
        let still_used = paths_in_use.reason();
        if let Some(wt_info) = &request.instance.worktree_info {
            let path = PathBuf::from(&request.instance.project_path);
            if wt_info.managed_by_aoe && !preserved_worktree_paths.contains(&path) && in_use(&path)
            {
                messages.push(format!("Worktree kept; {still_used}"));
                preserved_worktree_paths.insert(path);
            }
        }
        if let Some(ws_info) = &request.instance.workspace_info {
            // Sessions attached to a workspace work in its root, so any use under it keeps every
            // repo worktree.
            if in_use(Path::new(&ws_info.workspace_dir)) {
                for repo in repos.iter().filter(|r| r.managed_by_aoe) {
                    if preserved_worktree_paths.insert(PathBuf::from(&repo.worktree_path)) {
                        messages.push(format!(
                            "Workspace ({}) worktree kept; {still_used}",
                            repo.name
                        ));
                    }
                }
            }
        }
        for repo in repos.iter().filter(|repo| repo.managed_by_aoe) {
            let path = PathBuf::from(&repo.worktree_path);
            if in_use(&path) && preserved_worktree_paths.insert(path) {
                messages.push(format!(
                    "Workspace ({}) worktree kept; {still_used}",
                    repo.name
                ));
            }
        }
    }

    if request.delete_worktree && !request.force_delete {
        if let Some(wt_info) = &request.instance.worktree_info {
            if wt_info.managed_by_aoe {
                let path = PathBuf::from(&request.instance.project_path);
                // A path the guard above already preserved must not also report dirty: that error
                // would fail the deletion and strand the row in the trash, which is what the guard
                // exists to avoid.
                if !preserved_worktree_paths.contains(&path) {
                    if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                        tracing::debug!(target: "session.delete",
                            session_id = %request.session_id,
                            path = %path.display(),
                            "perform_deletion: dirty worktree, skipping preclean + host remove"
                        );
                        errors.push(format!("Worktree: {}", msg));
                        preserved_worktree_paths.insert(path);
                    }
                }
            }
        }
        for repo in repos.iter().filter(|r| r.managed_by_aoe) {
            let path = PathBuf::from(&repo.worktree_path);
            if preserved_worktree_paths.contains(&path) {
                continue;
            }
            if let Some(msg) = crate::git::cleanup::dirty_worktree_message(&path) {
                tracing::debug!(target: "session.delete",
                    session_id = %request.session_id,
                    repo = %repo.name,
                    path = %path.display(),
                    "perform_deletion: dirty session repo, skipping preclean + host remove"
                );
                errors.push(format!("Workspace ({}): {}", repo.name, msg));
                preserved_worktree_paths.insert(path);
            }
        }
    }

    preserved_worktree_paths
}

/// Map a container [`Teardown`](crate::containers::Teardown) outcome onto a deletion's user-facing
/// messages and errors.
fn deletion_messages_for(
    outcome: crate::containers::Teardown,
    messages: &mut Vec<String>,
    errors: &mut Vec<String>,
) {
    use crate::containers::Teardown;
    match outcome {
        Teardown::Removed => messages.push("Container removed".to_string()),
        Teardown::AlreadyGone => {}
        Teardown::Failed(e) => errors.push(format!("Container: {}", e)),
    }
}

/// Run on_destroy hooks for an instance.
fn run_on_destroy_hooks(instance: &Instance, detach: bool) {
    let profile = crate::session::config::effective_profile(&instance.source_profile);

    let project_path = Path::new(&instance.project_path);

    // Start with global+profile on_destroy hooks (implicitly trusted).
    let mut resolved_on_destroy =
        crate::session::config::profile_config::resolve_config_or_warn(&profile)
            .hooks
            .on_destroy;

    // Check if repo has trusted hooks that override. Only the hooks surface
    // matters here; untrusted project MCP must not suppress trusted hooks.
    match repo_config::check_repo_trust(project_path) {
        Ok(trust) if trust.hooks.needs_trust() => {
            tracing::warn!(target: "session.delete",
                "Repo hooks changed since last trust approval; skipping repo on_destroy hooks"
            );
        }
        Ok(trust) => {
            if let Some(hooks) = trust.hooks.trusted() {
                if !hooks.on_destroy.is_empty() {
                    resolved_on_destroy = hooks.on_destroy;
                }
            }
        }
        Err(_) => {}
    }

    if resolved_on_destroy.is_empty() {
        return;
    }

    tracing::info!(target: "session.delete", "Running on_destroy hooks for session {}", instance.id);

    let is_sandboxed = instance.sandbox_info.as_ref().is_some_and(|s| s.enabled);
    let hook_env = repo_config::lifecycle_env_vars(instance);

    // The caller controls detachment: TUI/web pass detach=true to avoid corrupting the rendered UI
    // (see issue); CLI passes detach=false so interactive prompts work.
    let errors = if is_sandboxed {
        if let Some(ref sandbox) = instance.sandbox_info {
            let workdir = instance.container_workdir();
            repo_config::execute_hooks_in_container_best_effort(
                &resolved_on_destroy,
                &sandbox.container_name,
                &workdir,
                detach,
                &hook_env,
            )
        } else {
            vec![]
        }
    } else {
        repo_config::execute_hooks_best_effort(
            &resolved_on_destroy,
            project_path,
            detach,
            &hook_env,
        )
    };

    if !errors.is_empty() {
        tracing::warn!(target: "session.delete",
            "on_destroy hooks had {} failure(s) for session {}",
            errors.len(),
            instance.id
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::containers::error::DockerError;
    use crate::containers::Teardown;
    use crate::session::test_support::{isolate_app_dir, isolate_app_dir_at};
    use crate::session::{SandboxInfo, WorkspaceInfo, WorkspaceRepo, WorktreeInfo};
    use serial_test::serial;
    use std::fs;

    #[test]
    #[cfg(unix)]
    #[serial]
    fn strict_ownership_reader_never_treats_unreadable_or_broken_store_as_absent() {
        let _home = isolate_app_dir();
        let storage = Storage::new_unwatched("strict-io").unwrap();
        assert!(storage
            .load_strict_for_worktree_ownership()
            .unwrap()
            .is_empty());
        std::fs::create_dir(storage.sessions_path()).unwrap();
        assert!(storage.load_strict_for_worktree_ownership().is_err());
        std::fs::remove_dir(storage.sessions_path()).unwrap();
        std::os::unix::fs::symlink("missing.json", storage.sessions_path()).unwrap();
        assert!(storage.load_strict_for_worktree_ownership().is_err());
        let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
        assert!(matches!(
            paths_in_use_except_with_ownership(&ownership, "strict-io", &[]),
            PathsInUse::Unknown(_)
        ));
    }

    #[test]
    #[serial]
    fn ownership_inventory_rejects_invalid_and_ambiguous_rows_without_quarantine() {
        let _home = isolate_app_dir();
        let storage = Storage::new_unwatched("strict").unwrap();
        let valid = Instance::new("owner", "/claimed");
        for rows in [
            serde_json::json!([valid, {"id": "broken", "project_path": "/precious"}]),
            serde_json::json!([valid, valid]),
        ] {
            std::fs::write(storage.sessions_path(), serde_json::to_vec(&rows).unwrap()).unwrap();
            let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
            assert!(storage.load_strict_for_worktree_ownership().is_err());
            assert!(matches!(
                paths_in_use_except_with_ownership(&ownership, "strict", &[&valid.id]),
                PathsInUse::Unknown(_)
            ));
            assert!(!storage
                .sessions_path()
                .with_file_name("sessions.corrupt.jsonl")
                .exists());
        }
        // The forgiving UI reader remains deliberately available.
        std::fs::write(
            storage.sessions_path(),
            serde_json::to_vec(&serde_json::json!([valid, {"id": "broken"}])).unwrap(),
        )
        .unwrap();
        assert_eq!(storage.load().unwrap().len(), 1);
    }

    #[test]
    #[serial]
    fn acp_purge_preserves_another_profiles_identity_before_worker_or_sql_cleanup() {
        for identity in ["same-incarnation", "new-incarnation", "corrupt-inventory"] {
            let _home = isolate_app_dir();
            let owner_storage = Storage::new_unwatched("retired-owner").unwrap();
            owner_storage.update(|_, _| Ok(())).unwrap();
            let mut owner = Instance::new("removed", "/absent");
            owner.source_profile = owner_storage.profile().into();
            let peer = Storage::new_unwatched("surviving-owner").unwrap();
            if identity == "corrupt-inventory" {
                fs::write(peer.sessions_path(), b"unreadable registry").unwrap();
            } else {
                let mut survivor = owner.clone();
                survivor.source_profile = peer.profile().into();
                survivor.project_path = "/other-checkout".into();
                if identity == "new-incarnation" {
                    survivor.created_at += chrono::Duration::seconds(1);
                }
                peer.update(|rows, _| {
                    rows.push(survivor);
                    Ok(())
                })
                .unwrap();
            }
            // A read of this record would fail: a successful retention proves
            // the worker path was not entered for the surviving owner.
            let record = crate::process::worker_registry::record_path(&owner.id).unwrap();
            fs::create_dir_all(record.parent().unwrap()).unwrap();
            fs::write(&record, b"unreadable worker record").unwrap();
            let database = crate::session::get_app_dir().unwrap().join("acp_events.db");
            let conn = rusqlite::Connection::open(&database).unwrap();
            conn.execute_batch(
                "CREATE TABLE acp_events (session_id TEXT);
                CREATE TABLE acp_attachments (session_id TEXT);",
            )
            .unwrap();
            for table in ["acp_events", "acp_attachments"] {
                conn.execute(&format!("INSERT INTO {table} VALUES (?1)"), [&owner.id])
                    .unwrap();
            }
            let acp = execute_purge_effect(
                &owner,
                &ResourceEffect::PurgeAcpEvents { database },
                false,
                |_| panic!("no container cleanup"),
            );
            let sidecars = execute_purge_effect(
                &owner,
                &ResourceEffect::PurgeSidecars { stores: None },
                false,
                |_| panic!("no container cleanup"),
            );
            let retained = identity == "same-incarnation";
            assert_eq!(acp.is_ok(), retained, "{identity}: {acp:?}");
            assert_eq!(sidecars.is_ok(), retained, "{identity}: {sidecars:?}");
            if let Ok((messages, _)) = acp {
                assert!(messages
                    .iter()
                    .any(|message| message.contains("transcript kept")));
            }
            for table in ["acp_events", "acp_attachments"] {
                let count: i64 = conn
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                        row.get(0)
                    })
                    .unwrap();
                assert_eq!(count, 1, "{identity}: {table} was deleted");
            }
            assert_eq!(fs::read(record).unwrap(), b"unreadable worker record");
        }
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn external_profile_aliases_exclude_only_the_same_physical_owner() {
        let _home = isolate_app_dir();
        let external = tempfile::tempdir().unwrap();
        let profiles = crate::session::get_app_dir().unwrap().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        for alias in ["external-a", "external-b"] {
            std::os::unix::fs::symlink(external.path(), profiles.join(alias)).unwrap();
        }
        let mut owner = Instance::new("external owner", "/external-owned");
        owner.id = "same-id".into();
        std::fs::write(
            external.path().join("sessions.json"),
            serde_json::to_vec(&vec![owner.clone()]).unwrap(),
        )
        .unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        let mut peer_row = owner.clone();
        peer_row.project_path = "/peer-owned".into();
        peer.update(|rows, _| {
            rows.push(peer_row);
            Ok(())
        })
        .unwrap();
        assert_eq!(crate::session::list_profiles().unwrap(), vec!["peer"]);
        let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
        for alias in ["external-a", "external-b"] {
            let paths = paths_in_use_except_with_ownership(&ownership, alias, &["same-id"]);
            assert!(!paths.covers(Path::new("/external-owned")));
            assert!(paths.covers(Path::new("/peer-owned")));
        }
        assert_eq!(
            all_profile_storages_with_ownership(&ownership)
                .unwrap()
                .1
                .len(),
            2
        );
    }

    #[test]
    #[cfg(unix)]
    #[serial]
    fn raw_point_claims_protect_ancestors_without_reserving_children() {
        let _home = isolate_app_dir();
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        let alias = temp.path().join("alias");
        std::fs::create_dir(&real).unwrap();
        std::os::unix::fs::symlink(&real, &alias).unwrap();
        let peer = Storage::new_unwatched("peer").unwrap();
        peer.update(|rows, _| {
            rows.push(Instance::new("peer", alias.to_str().unwrap()));
            Ok(())
        })
        .unwrap();
        let ownership = crate::session::storage::acquire_ownership_lock().unwrap();
        let paths = paths_in_use_except_with_ownership(&ownership, "peer", &[]);
        assert!(paths.covers(&real));
        assert!(!paths.covers(&real.join("missing/child")));
        assert!(paths.covers(temp.path()));
        assert!(!paths.covers(&temp.path().join("unrelated")));
        assert_eq!(
            resolve_claim_path(&alias.join("missing/../child")),
            Some(real.canonicalize().unwrap().join("child"))
        );
        std::os::unix::fs::symlink(temp.path().join("gone"), temp.path().join("broken")).unwrap();
        assert!(paths.covers(&temp.path().join("broken/tail")));
        let profiles = crate::session::get_app_dir().unwrap().join("profiles");
        std::os::unix::fs::symlink(temp.path().join("gone"), profiles.join("broken-profile"))
            .unwrap();
        assert!(matches!(
            paths_in_use_except_with_ownership(&ownership, "peer", &[]),
            PathsInUse::Unknown(_)
        ));
    }

    #[test]
    fn late_profile_process_helper() {
        use std::io::Write;
        let Some(home) = std::env::var_os("AOE_LATE_PROFILE_HOME") else {
            return;
        };
        let _home = isolate_app_dir_at(Path::new(&home));
        let app = crate::session::get_app_dir().unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(app.join(".workspace-claim.lock"))
            .unwrap();
        fs2::FileExt::try_lock_exclusive(&file).unwrap();
        drop(file);
        let storage = Storage::new_unwatched("late-b").unwrap();
        let target = PathBuf::from(std::env::var_os("AOE_LATE_PROFILE_TARGET_DIR").unwrap());
        assert!(
            target.exists(),
            "parent removed the checkout before the adoption attempt"
        );
        assert!(storage
            .update(|rows, _| {
                rows.push(Instance::new("B", &target.to_string_lossy()));
                Ok(())
            })
            .is_err());
        assert!(storage.load().unwrap().is_empty());
        assert!(target.exists());
        println!("ADOPTION_REFUSED");
        std::io::stdout().flush().unwrap();
    }

    #[test]
    #[serial]
    fn late_profile_after_scan_cannot_adopt_the_reserved_checkout_before_removal() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/late-b");
        let home = temp.path().join("home");
        let _home = isolate_app_dir_at(&home);
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let target = worktree.clone();
        BEFORE_COMMITTED_PURGE_RESUME.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                let mut child = std::process::Command::new(std::env::current_exe().unwrap());
                child
                    .args([
                        "--exact",
                        "session::deletion::tests::late_profile_process_helper",
                        "--nocapture",
                    ])
                    .env("AOE_LATE_PROFILE_HOME", &home)
                    .env("AOE_LATE_PROFILE_TARGET_DIR", &target);
                let output = crate::process::run_with_timeout_process_group(
                    &mut child,
                    std::time::Duration::from_secs(5),
                )
                .unwrap()
                .expect("child did not complete its adoption attempt");
                assert!(
                    output.status.success(),
                    "{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                assert!(String::from_utf8_lossy(&output.stdout).contains("ADOPTION_REFUSED"));
            }));
        });
        let transaction = match PurgeTransaction::reserve(
            storage,
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let result = transaction.complete();
        assert!(result.success, "{:?}", result.errors);
        assert!(!worktree.exists());
        assert!(!branch_exists(&repo, "feature/late-b"));
        assert!(Storage::open_unwatched("late-b")
            .unwrap()
            .load()
            .unwrap()
            .is_empty());
    }

    #[test]
    #[serial]
    fn purge_rechecks_dirty_checkout_after_hooks_before_real_git_removal() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/hook-refresh");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        let owner_id = owner.id.clone();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let result = transaction
            .run_hooks_with(|_, _| {
                std::fs::write(worktree.join("keep"), "hook data").unwrap();
            })
            .complete();
        assert!(!result.success);
        assert_eq!(
            std::fs::read_to_string(worktree.join("keep")).unwrap(),
            "hook data"
        );
        assert!(branch_exists(&repo, "feature/hook-refresh"));
        let rows = storage.load().unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec![owner_id.as_str()]
        );
    }

    #[test]
    #[serial]
    fn committed_purge_allows_sdk_annotations_and_rejects_new_claim_before_git() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/committed");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        let other = Instance::new("SDK peer", "/unrelated");
        let other_id = other.id.clone();
        storage
            .update(|rows, _| {
                rows.extend([owner.clone(), other]);
                Ok(())
            })
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let committed = transaction
            .begin_irreversible()
            .unwrap_or_else(|_| panic!("purge commit rejected"));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let writer_storage = storage.clone();
        let adopted_path = worktree.clone();
        let writer = std::thread::spawn(move || {
            writer_storage
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == other_id)
                        .unwrap()
                        .title = "SDK metadata committed".into();
                    Ok(())
                })
                .unwrap();
            assert!(writer_storage
                .update(|rows, _| {
                    rows.iter_mut()
                        .find(|row| row.id == other_id)
                        .unwrap()
                        .project_path = adopted_path.to_string_lossy().into_owned();
                    Ok(())
                })
                .is_err());
            done_tx.send(()).unwrap();
            other_id
        });
        let published_before_finish = done_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .is_ok();
        let result = committed.finish();
        let other_id = writer.join().unwrap();
        assert!(
            published_before_finish,
            "committed purge blocked unrelated SDK metadata"
        );
        assert!(result.success, "{:?}", result.errors);
        assert!(!worktree.exists());
        assert!(!branch_exists(&repo, "feature/committed"));
        let rows = storage.load().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, other_id);
        assert_eq!(rows[0].title, "SDK metadata committed");
        assert_eq!(rows[0].project_path, "/unrelated");
    }

    #[test]
    #[serial]
    #[serial(hook_base)]
    fn committed_purge_keeps_its_profile_fenced_until_cleanup_finishes() {
        let (_hooks, _base, _hooks_dir) = crate::hooks::test_support::BaseGuard::ready();
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/profile-gap");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        let sidecar = crate::hooks::ensure_instance_dir_path(&owner.id).unwrap();
        std::fs::write(sidecar.join("status"), "working").unwrap();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let committed = transaction
            .begin_irreversible()
            .unwrap_or_else(|_| panic!("commit rejected"));
        assert!(storage.load().unwrap().is_empty());
        BEFORE_COMMITTED_PURGE_RESUME.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(|| {
                assert!(crate::session::rename_profile("owner", "renamed").is_err());
            }));
        });
        let result = committed.finish();
        assert!(result.success, "{:?}", result.errors);
        assert_eq!(result.disposition, DeletionDisposition::Removed);
        assert!(!sidecar.exists());
        assert!(!worktree.exists());
        assert!(!branch_exists(&repo, "feature/profile-gap"));
        crate::session::rename_profile("owner", "renamed").unwrap();
    }

    #[test]
    #[serial]
    #[serial(hook_base)]
    fn committed_purge_preserves_sidecars_when_identity_is_reused_or_inventory_unknown() {
        for corrupt in [false, true] {
            let (_hooks, _base, _hooks_dir) = crate::hooks::test_support::BaseGuard::ready();
            let (temp, _repo, _worktree, mut owner) = worktree_fixture("feature/identity-gap");
            let _home = isolate_app_dir_at(&temp.path().join("home"));
            let storage = Storage::new_unwatched("owner").unwrap();
            let peer = Storage::new_unwatched("peer").unwrap();
            owner.source_profile = "owner".into();
            let id = owner.id.clone();
            let sidecar = crate::hooks::ensure_instance_dir_path(&id).unwrap();
            std::fs::write(sidecar.join("status"), "working").unwrap();
            storage
                .update(|rows, _| {
                    rows.push(owner.clone());
                    Ok(())
                })
                .unwrap();
            let transaction = match PurgeTransaction::reserve(
                storage.clone(),
                DeletionRequest {
                    delete_worktree: true,
                    delete_branch: true,
                    ..request(owner)
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("purge rejected"),
            };
            let committed = transaction
                .begin_irreversible()
                .unwrap_or_else(|_| panic!("commit rejected"));
            let peer_file = peer.sessions_path().to_path_buf();
            let bytes = if corrupt {
                b"invalid owner rows".to_vec()
            } else {
                let mut replacement = Instance::new("new owner", "/unrelated");
                replacement.id = id;
                serde_json::to_vec(&[replacement]).unwrap()
            };
            let fixture_bytes = bytes.clone();
            BEFORE_COMMITTED_PURGE_RESUME.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    std::fs::write(peer_file, fixture_bytes).unwrap();
                }));
            });
            let result = committed.finish();
            assert_eq!(result.disposition, DeletionDisposition::Removed);
            assert!(!result.success);
            assert_eq!(std::fs::read(sidecar.join("status")).unwrap(), b"working");
            assert_eq!(std::fs::read(peer.sessions_path()).unwrap(), bytes);
            std::fs::remove_dir_all(sidecar).unwrap();
        }
    }

    #[test]
    #[serial]
    fn purge_admission_preserves_unreadable_owner_rows_before_metadata_mutation() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/strict-admission");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        let mut unreadable = serde_json::to_value(&owner).unwrap();
        unreadable["id"] = serde_json::json!("unreadable-peer");
        unreadable["status"] = serde_json::json!("invalid-status");
        let original = serde_json::to_vec(&serde_json::json!([owner.clone(), unreadable])).unwrap();
        std::fs::write(storage.sessions_path(), &original).unwrap();
        let result = PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                session_id: owner.id.clone(),
                instance: owner,
                delete_worktree: true,
                delete_branch: true,
                delete_sandbox: false,
                force_delete: true,
                detach_hooks: false,
                keep_scratch: false,
            },
        );
        assert!(
            result.is_err(),
            "purge admission rewrote an incomplete inventory"
        );
        assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), original);
        assert!(worktree.is_dir());
        assert!(branch_exists(&repo, "feature/strict-admission"));
    }

    #[test]
    #[serial]
    fn purge_exclusion_is_profile_qualified_for_paths_and_branches() {
        for branch_only in [false, true] {
            let (temp, repo, worktree, mut owner) = worktree_fixture("feature/profile-owner");
            let _home = isolate_app_dir_at(&temp.path().join("home"));
            let storage = Storage::new_unwatched("owner").unwrap();
            owner.source_profile = "owner".into();
            storage
                .update(|rows, _| {
                    rows.push(owner.clone());
                    Ok(())
                })
                .unwrap();
            let other = Storage::new_unwatched("other").unwrap();
            let mut alias = owner.clone();
            alias.source_profile = "other".into();
            if branch_only {
                alias.project_path = repo.to_string_lossy().into_owned();
            }
            other
                .update(|rows, _| {
                    rows.push(alias);
                    Ok(())
                })
                .unwrap();
            let transaction = match PurgeTransaction::reserve(
                storage,
                DeletionRequest {
                    delete_worktree: true,
                    delete_branch: true,
                    ..request(owner)
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("purge rejected"),
            };
            let result = transaction.complete();
            assert!(result.success, "{:?}", result.errors);
            assert_eq!(worktree.exists(), !branch_only);
            assert!(branch_exists(&repo, "feature/profile-owner"));
            assert_eq!(other.load().unwrap().len(), 1);
        }
    }

    #[test]
    #[serial]
    fn purge_reservation_blocks_profile_rename_during_hooks() {
        let (temp, repo, worktree, mut owner) = worktree_fixture("feature/profile-refresh");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".into();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let transaction = match PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap()
        {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge rejected"),
        };
        let result = transaction
            .run_hooks_with(|_, _| {
                let before = std::fs::read(storage.sessions_path()).unwrap();
                assert!(crate::session::rename_profile("owner", "original-owner").is_err());
                assert_eq!(std::fs::read(storage.sessions_path()).unwrap(), before);
                assert!(Storage::open_unwatched("original-owner").is_err());
            })
            .complete();
        assert!(result.success, "{:?}", result.errors);
        assert!(!worktree.exists());
        assert!(!branch_exists(&repo, "feature/profile-refresh"));
        assert!(storage.load().unwrap().is_empty());
    }
    fn request(instance: Instance) -> DeletionRequest {
        DeletionRequest {
            session_id: instance.id.clone(),
            instance,
            delete_worktree: false,
            delete_branch: false,
            delete_sandbox: false,
            force_delete: false,
            detach_hooks: true,
            keep_scratch: false,
        }
    }

    fn persisted_request(mut request: DeletionRequest) -> DeletionRequest {
        let profile = if request.instance.source_profile.is_empty() {
            "default"
        } else {
            &request.instance.source_profile
        };
        let storage = Storage::new_unwatched(profile).unwrap();
        request.instance.source_profile = storage.profile().to_owned();
        storage
            .update(|rows, _| {
                rows.push(request.instance.clone());
                Ok(())
            })
            .unwrap();
        request
    }

    fn sandbox_info(container_name: &str) -> SandboxInfo {
        SandboxInfo {
            provider: None,
            enabled: true,
            container_id: None,
            image: "alpine".to_string(),
            container_name: container_name.to_string(),
            extra_env: None,
            custom_instruction: None,
            before_start_env: Vec::new(),
            container_workdir: None,
        }
    }

    fn worktree_info(branch: &str, main_repo: &Path) -> WorktreeInfo {
        WorktreeInfo {
            branch: branch.to_string(),
            main_repo_path: main_repo.to_string_lossy().to_string(),
            managed_by_aoe: true,
            created_at: chrono::Utc::now(),
            base_branch: None,
        }
    }

    fn workspace_repo(main_repo: &Path, worktree: &Path, branch: &str) -> WorkspaceRepo {
        WorkspaceRepo {
            name: main_repo
                .file_name()
                .map_or("repo".to_string(), |n| n.to_string_lossy().to_string()),
            source_path: main_repo.to_string_lossy().to_string(),
            branch: branch.to_string(),
            worktree_path: worktree.to_string_lossy().to_string(),
            main_repo_path: main_repo.to_string_lossy().to_string(),
            managed_by_aoe: true,
            branch_preexisting: false,
            base_branch: None,
            base_branch_override: None,
        }
    }

    fn workspace_info(workspace_dir: &Path, repos: Vec<WorkspaceRepo>) -> WorkspaceInfo {
        WorkspaceInfo {
            branch: repos
                .first()
                .map_or("feature/abc".to_string(), |repo| repo.branch.clone()),
            workspace_dir: workspace_dir.to_string_lossy().to_string(),
            repos,
            created_at: chrono::Utc::now(),
            cleanup_on_delete: true,
        }
    }

    fn init_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        let repo = git2::Repository::init(path).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();
        let tree_id = repo.index().unwrap().write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
    }

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn branch_exists(repo: &Path, branch: &str) -> bool {
        !git_in(repo, &["branch", "--list", branch]).is_empty()
    }

    /// A repo at `<tmp>/main` with a managed worktree for `branch` at `<tmp>/worktree`.
    fn worktree_fixture(branch: &str) -> (tempfile::TempDir, PathBuf, PathBuf, Instance) {
        let tmp = tempfile::TempDir::new().unwrap();
        let main_repo = tmp.path().join("main");
        let worktree_path = tmp.path().join("worktree");
        init_repo(&main_repo);
        let worktree = worktree_path.to_str().unwrap();
        git_in(&main_repo, &["worktree", "add", "-b", branch, worktree]);
        let mut instance = Instance::new("Test", worktree);
        instance.worktree_info = Some(worktree_info(branch, &main_repo));
        (tmp, main_repo, worktree_path, instance)
    }

    #[test]
    #[serial]
    fn branch_cleanup_keeps_a_branch_without_registered_checkout_ownership() {
        let _home = isolate_app_dir();
        let storage = Storage::new_unwatched("default").unwrap();
        let (fixture, main, path, mut owner) = worktree_fixture("feature/recovery");
        let _tmux = crate::session::test_support::EnvGuard::set(&[(
            "AOE_TMUX_SOCKET",
            fixture.path().join("tmux.sock"),
        )]);
        let branch = owner.worktree_info.as_ref().unwrap().branch.clone();
        GitWorktree::new(main.clone())
            .unwrap()
            .remove_worktree(&path, false)
            .unwrap();
        owner.source_profile = storage.profile().to_owned();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let result = execute_deletion(DeletionRequest {
            delete_worktree: true,
            delete_branch: true,
            ..request(owner)
        });
        assert!(result.success, "{:?}", result.errors);
        assert_eq!(result.disposition, DeletionDisposition::Removed);
        assert!(storage.load().unwrap().is_empty());
        assert!(branch_exists(&main, &branch));
        assert!(!path.exists());
    }

    fn reserve(profile: &str, instance: Instance) -> PurgeTransaction {
        let storage = Storage::open_unwatched(profile).unwrap();
        match PurgeTransaction::reserve(storage, request(instance)).unwrap() {
            PurgeReservation::Reserved(transaction) => transaction,
            PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
        }
    }

    fn stored_instance(storage: &Storage, profile: &str, project: &str) -> Instance {
        let mut instance = Instance::new("purge", project);
        instance.source_profile = profile.to_string();
        storage
            .update(|instances, _groups| {
                instances.push(instance.clone());
                Ok(())
            })
            .unwrap();
        instance
    }

    #[test]
    #[serial]
    fn rowless_purge_replays_prepared_cleanup_and_observed_failure_but_not_pending() {
        use super::super::move_journal::{self, MoveAuthority};
        for interrupted in ["prepared", "observed-error", "pending", "conflict"] {
            let uncertain = matches!(interrupted, "pending" | "conflict");
            let _home = isolate_app_dir();
            let temp = tempfile::tempdir().unwrap();
            let _tmux = crate::session::test_support::EnvGuard::set(&[(
                "AOE_TMUX_SOCKET",
                temp.path().join("tmux.sock"),
            )]);
            let storage = Storage::new_unwatched("rowless-owner").unwrap();
            let owner = stored_instance(&storage, "rowless-owner", &temp.path().to_string_lossy());
            let db = crate::session::get_app_dir().unwrap().join("acp_events.db");
            let conn = rusqlite::Connection::open(&db).unwrap();
            conn.execute_batch(
                "CREATE TABLE acp_events (session_id TEXT);
                CREATE TABLE acp_attachments (session_id TEXT);",
            )
            .unwrap();
            for table in ["acp_events", "acp_attachments"] {
                conn.execute(&format!("INSERT INTO {table} VALUES (?1)"), [&owner.id])
                    .unwrap();
                conn.execute(&format!("INSERT INTO {table} VALUES ('other')"), [])
                    .unwrap();
            }
            let committed = reserve("rowless-owner", owner.clone())
                .run_hooks_with(|_, _| {})
                .begin_irreversible()
                .unwrap_or_else(|error| panic!("{error:?}"));
            assert!(storage.load().unwrap().is_empty());
            let transaction = &committed.transaction.transaction;
            let index = 2;
            match interrupted {
                "observed-error" => {
                    conn.execute_batch("BEGIN IMMEDIATE").unwrap();
                    let effect = transaction.effect(index).unwrap();
                    let failed = transaction
                        .run_effect(index, || {
                            execute_purge_effect(&owner, &effect, false, |_| panic!("no container"))
                        })
                        .unwrap();
                    assert!(failed.is_err());
                    conn.execute_batch("ROLLBACK").unwrap();
                }
                "pending" | "conflict" => {
                    let root = acquire_ownership_lock().unwrap();
                    transaction
                        .begin_effect_with_ownership(&root, index)
                        .unwrap();
                    if interrupted == "conflict" {
                        let mut entry = transaction.journal().read().unwrap();
                        let MoveAuthority::Resource { intent } = &mut entry.authority else {
                            unreachable!()
                        };
                        intent.phase = super::super::move_journal::EffectPhase::Conflict;
                        intent.effects[index].phase =
                            super::super::move_journal::EffectPhase::Conflict;
                        intent.conflict = Some("effect completion could not be observed".into());
                        transaction.journal().rewrite(&entry).unwrap();
                    }
                }
                _ => {}
            }
            drop(committed);
            for _ in 0..2 {
                let warnings =
                    Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
                        .unwrap();
                assert_eq!(warnings.is_empty(), !uncertain, "{warnings:?}");
            }
            let count: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM acp_events WHERE session_id = ?1",
                    [&owner.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, i64::from(uncertain));
            let other: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM acp_events WHERE session_id = 'other'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(other, 1);
            let pending = move_journal::scan();
            assert_eq!(pending.entries.is_empty(), !uncertain);
            if uncertain {
                let MoveAuthority::Resource { intent } =
                    &pending.entries[0].1.as_ref().unwrap().authority
                else {
                    unreachable!()
                };
                let expected = if interrupted == "pending" {
                    super::super::move_journal::EffectPhase::Pending
                } else {
                    super::super::move_journal::EffectPhase::Conflict
                };
                assert_eq!(intent.effects[index].phase, expected);
            }
        }
    }

    #[test]
    fn purge_container_dispatch_records_only_successful_release() {
        let instance = Instance::new("container", "/absent");
        for outcome in [
            crate::containers::Teardown::Removed,
            crate::containers::Teardown::AlreadyGone,
            crate::containers::Teardown::Failed(std::io::Error::other("temporary failure").into()),
        ] {
            let succeeds = !matches!(outcome, crate::containers::Teardown::Failed(_));
            let result =
                execute_purge_effect(&instance, &ResourceEffect::ReleaseContainer, true, |_| {
                    Ok(outcome)
                });
            assert_eq!(result.is_ok(), succeeds);
            if let Ok((_, Some(observation))) = result {
                assert_eq!(observation.container_released, Some(true));
            }
        }
    }

    #[test]
    #[serial]
    fn rowless_prepared_workspace_cleanup_reuses_the_producer_dispatcher() {
        let (temp, repo, old, mut owner) = worktree_fixture("feature/prepared-workspace");
        let _home = isolate_app_dir_at(&temp.path().join("home"));
        let _tmux = crate::session::test_support::EnvGuard::set(&[(
            "AOE_TMUX_SOCKET",
            temp.path().join("tmux.sock"),
        )]);
        let workspace = temp.path().join("workspace");
        fs::create_dir(&workspace).unwrap();
        let checkout = workspace.join("project");
        GitWorktree::new(repo.clone())
            .unwrap()
            .move_worktree(&old, &checkout)
            .unwrap();
        owner.project_path = checkout.to_string_lossy().into_owned();
        owner.workspace_info = Some(workspace_info(
            &workspace,
            vec![workspace_repo(
                &repo,
                &checkout,
                "feature/prepared-workspace",
            )],
        ));
        owner.worktree_info = None;
        owner.source_profile = "prepared-workspace-owner".into();
        let storage = Storage::new_unwatched(&owner.source_profile).unwrap();
        storage
            .update(|rows, _| {
                rows.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let PurgeReservation::Reserved(transaction) = PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        )
        .unwrap() else {
            panic!("purge was refused")
        };
        let committed = transaction
            .run_hooks_with(|_, _| {})
            .begin_irreversible()
            .unwrap_or_else(|error| panic!("{error:?}"));
        drop(committed);
        for _ in 0..2 {
            assert!(
                Storage::recover_pending_moves(crate::file_watch::FileWatchService::noop())
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(!checkout.exists());
        assert!(!workspace.exists());
        assert!(!branch_exists(&repo, "feature/prepared-workspace"));
        assert!(storage.load().unwrap().is_empty());
        assert!(super::super::move_journal::scan().entries.is_empty());
    }

    /// A force chosen against an earlier trash lifecycle is dropped at the claim, so a row
    /// a peer restored and re-trashed keeps its dirty-worktree guard.
    #[test]
    #[serial]
    fn purge_claim_drops_force_from_a_stale_trash_lifecycle() {
        let _guard = isolate_app_dir();
        let earlier = chrono::Utc::now() - chrono::Duration::minutes(5);
        // (profile, lifecycle the request was chosen against matches the stored one)
        for (profile, same_lifecycle) in [("purge-stale-force", false), ("purge-same-force", true)]
        {
            let storage = Storage::new_unwatched(profile).unwrap();
            let mut requested = stored_instance(&storage, profile, "/tmp/test-project");
            let trashed_at = chrono::Utc::now();
            storage
                .update(|instances, _groups| {
                    instances[0].trashed_at = Some(trashed_at);
                    Ok(())
                })
                .unwrap();
            requested.trashed_at = Some(if same_lifecycle { trashed_at } else { earlier });
            let transaction = match PurgeTransaction::reserve(
                storage,
                DeletionRequest {
                    force_delete: true,
                    ..request(requested)
                },
            )
            .unwrap()
            {
                PurgeReservation::Reserved(transaction) => transaction,
                PurgeReservation::Rejected(_) => panic!("purge reservation was refused"),
            };
            assert_eq!(
                transaction.request.force_delete, same_lifecycle,
                "{profile}"
            );
        }
    }

    #[test]
    #[serial]
    fn purge_transaction_generation_gate_and_durable_commit() {
        let _guard = isolate_app_dir();
        let profile = "purge-generation-gate";
        let storage = Storage::new_unwatched(profile).unwrap();
        let transaction = reserve(
            profile,
            stored_instance(&storage, profile, "/tmp/test-project"),
        );
        let reserved = storage.load().unwrap().remove(0);
        assert!(storage
            .update(|rows, _| {
                rows[0].lifecycle_generation += 1;
                Ok(())
            })
            .is_err());
        let current = storage.load().unwrap().remove(0);
        assert_eq!(current.created_at, reserved.created_at);
        assert_eq!(current.lifecycle_generation, reserved.lifecycle_generation);
        assert_eq!(
            current.lifecycle_reservation,
            reserved.lifecycle_reservation
        );
        let committed = transaction
            .begin_irreversible()
            .unwrap_or_else(|_| panic!("current purge rejected"));
        assert!(
            storage.load().unwrap().is_empty(),
            "row must be gone before irreversible cleanup"
        );
        let result = committed.finish();
        assert!(result.success, "{:?}", result.errors);
        assert_eq!(result.disposition, DeletionDisposition::Removed);
    }

    #[test]
    #[serial]
    fn on_destroy_hooks_run_without_the_instance_lifecycle_flock() {
        let temp = tempfile::tempdir().unwrap();
        let _home = isolate_app_dir_at(temp.path());
        let profile = "purge-unlocked-hooks";
        let storage = Storage::new_unwatched(profile).unwrap();
        let instance = stored_instance(&storage, profile, temp.path().to_str().unwrap());
        let id = instance.id.clone();
        let transaction = reserve(profile, instance);

        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let purge = std::thread::spawn(move || {
            transaction
                .run_hooks_with(|_, _| {
                    ready_tx.send(()).unwrap();
                    let _ = release_rx.recv();
                })
                .complete()
        });
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .expect("on_destroy hook did not start");

        let (lock_tx, lock_rx) = std::sync::mpsc::channel();
        let lock = std::thread::spawn(move || {
            let storage = Storage::open_unwatched(profile).unwrap();
            drop(storage.acquire_instance_lifecycle_lock(&id).unwrap());
            lock_tx.send(()).unwrap();
        });
        let acquired = lock_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .is_ok();
        release_tx.send(()).unwrap();

        let result = purge.join().unwrap();
        lock.join().unwrap();
        assert!(acquired, "on_destroy hook held the lifecycle flock");
        assert_eq!(result.disposition, DeletionDisposition::Removed);
        assert!(storage.load().unwrap().is_empty());
    }

    #[test]
    #[serial]
    fn purge_rejects_unreadable_profiles_before_any_effects() {
        let (tmp, main_repo, worktree, mut owner) = worktree_fixture("feature/shared");
        let _home = isolate_app_dir_at(&tmp.path().join("home"));
        let storage = Storage::new_unwatched("owner").unwrap();
        owner.source_profile = "owner".to_string();
        storage
            .update(|instances, _| {
                instances.push(owner.clone());
                Ok(())
            })
            .unwrap();
        let other = Storage::new_unwatched("other").unwrap();
        other
            .update(|instances, _| {
                instances.push(Instance::new("adopter", worktree.to_str().unwrap()));
                Ok(())
            })
            .unwrap();
        std::fs::write(other.sessions_path(), "not json").unwrap();
        let owner_id = owner.id.clone();
        let generation = owner.lifecycle_generation;
        let result = PurgeTransaction::reserve(
            storage.clone(),
            DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(owner)
            },
        );
        assert!(result.is_err());
        let rows = storage.load().unwrap();
        assert_eq!(
            rows.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            vec![owner_id.as_str()]
        );
        assert_eq!(rows[0].lifecycle_generation, generation);
        assert!(worktree.exists());
        assert!(branch_exists(&main_repo, "feature/shared"));
    }

    #[test]
    fn workspace_dir_ownership() {
        let owned = |dir: &str, worktrees: &[&str]| {
            let repos = worktrees
                .iter()
                .map(|wt| workspace_repo(Path::new("/src/repo"), Path::new(wt), "feature/abc"))
                .collect();
            workspace_dir_is_aoe_owned(&workspace_info(Path::new(dir), repos))
        };
        assert!(owned("/tmp/ws", &["/tmp/ws/backend", "/tmp/ws/frontend"]));
        // `workspace_dir` IS the user's checkout rather than a directory above it.
        assert!(!owned("/home/u/backend", &["/home/u/backend"]));
        assert!(!owned(
            "/tmp/ws",
            &["/tmp/ws/backend", "/elsewhere/frontend"]
        ));
        assert!(!owned("/tmp/ws", &[]));
    }

    mod container_removal {
        use super::*;

        fn sandboxed_request() -> DeletionRequest {
            let mut instance = Instance::new("Test Session", "/tmp/test-project");
            instance.sandbox_info = Some(sandbox_info("aoe-sandbox-calltest"));
            DeletionRequest {
                delete_sandbox: true,
                ..request(instance)
            }
        }

        /// The agent store goes only when a current session's purge fully succeeds.
        #[test]
        #[serial]
        fn agent_store_removal() {
            #[derive(Clone, Copy, Debug, PartialEq)]
            enum Case {
                Removed,
                PreTransition,
                FailedTeardown,
                FailsAfterTeardown,
            }
            for case in [
                Case::Removed,
                Case::PreTransition,
                Case::FailedTeardown,
                Case::FailsAfterTeardown,
            ] {
                let temp = tempfile::TempDir::new().unwrap();
                let _home = isolate_app_dir_at(temp.path());
                let mut request = sandboxed_request();
                if case == Case::PreTransition {
                    request.instance.sandbox_store_generation = 0;
                }
                if case == Case::FailsAfterTeardown {
                    request.delete_worktree = true;
                    request.instance.worktree_info =
                        Some(worktree_info("feature/x", &temp.path().join("not-a-repo")));
                }
                let store = temp
                    .path()
                    .join(".claude/sandbox-v2")
                    .join(&request.instance.id);
                std::fs::create_dir_all(&store).unwrap();
                std::fs::write(store.join(".credentials.json"), b"token").unwrap();
                if case != Case::PreTransition {
                    crate::migrations::v033_isolate_sandbox_content::certify_owned_test_root(
                        &crate::session::get_app_dir().unwrap(),
                        &request.instance.id,
                        &store,
                    )
                    .unwrap();
                }

                let request = persisted_request(request);
                let result = perform_deletion_with(&request, |_id| match case {
                    Case::FailedTeardown => Teardown::Failed(DockerError::DaemonNotRunning),
                    _ => Teardown::Removed,
                });

                if case == Case::Removed {
                    assert!(!store.exists(), "purge left the session's agent store");
                    assert!(result.success, "{:?}", result.errors);
                    assert!(result.messages.iter().any(|m| m.contains("Agent store")));
                } else {
                    assert!(store.exists(), "{case:?}: {:?}", result.errors);
                    assert!(case == Case::PreTransition || !result.success, "{case:?}");
                }
            }
        }
    }

    mod ordering {
        use super::*;

        #[test]
        fn real_worktree_and_branch_are_removed() {
            let _app_guard = isolate_app_dir();
            let (_tmp, main_repo, worktree_path, instance) = worktree_fixture("feature/delete-me");
            let request = persisted_request(DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(instance)
            });
            let result = perform_deletion(&request);
            assert!(result.success, "{:?}", result.errors);
            assert!(!worktree_path.exists());
            assert!(!branch_exists(&main_repo, "feature/delete-me"));
            assert!(Storage::open_unwatched("default")
                .unwrap()
                .load()
                .unwrap()
                .is_empty());
        }

        // Regression: the bare-repo layout checks out the default branch as a linked worktree.
        #[test]
        fn default_branch_worktree_survives_a_forced_delete() {
            let _app_guard = isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let bare = tmp.path().join("project/.bare");
            let worktree_path = tmp.path().join("project/main");
            std::fs::create_dir_all(&bare).unwrap();

            let repo = git2::Repository::init_bare(&bare).unwrap();
            let sig = git2::Signature::now("Test", "test@example.com").unwrap();
            let tree_id = {
                let blob = repo.blob(b"hello").unwrap();
                let mut tb = repo.treebuilder(None).unwrap();
                tb.insert("file.txt", blob, 0o100644).unwrap();
                tb.write().unwrap()
            };
            let tree = repo.find_tree(tree_id).unwrap();
            repo.commit(Some("refs/heads/main"), &sig, &sig, "init", &tree, &[])
                .unwrap();
            repo.set_head("refs/heads/main").unwrap();
            let worktree = worktree_path.to_str().unwrap();
            git_in(&bare, &["worktree", "add", worktree, "main"]);

            let mut instance = Instance::new("Infra", worktree);
            instance.worktree_info = Some(worktree_info("main", &bare));
            let result = perform_deletion(&persisted_request(DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                force_delete: true,
                ..request(instance)
            }));

            assert!(
                result.success,
                "deletion must still succeed: {:?}",
                result.errors
            );
            assert!(worktree_path.exists());
            assert!(branch_exists(&bare, "main"));
            assert_eq!(git_in(&bare, &["symbolic-ref", "HEAD"]), "refs/heads/main");
        }

        // The ownership guard must be consulted at the call site and its refusal surfaced.
        #[test]
        fn workspace_dir_that_is_not_aoe_owned_is_refused() {
            let _app_guard = isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let user_checkout = tmp.path().join("backend");
            init_repo(&user_checkout);
            let precious = user_checkout.join("uncommitted.txt");
            std::fs::write(&precious, "do not delete me").unwrap();

            let mut instance = Instance::new("Bad", user_checkout.to_str().unwrap());
            let mut repo = workspace_repo(&user_checkout, &user_checkout, "feature/abc");
            repo.managed_by_aoe = false;
            instance.workspace_info = Some(workspace_info(&user_checkout, vec![repo]));
            let result = perform_deletion(&persisted_request(DeletionRequest {
                delete_worktree: true,
                ..request(instance)
            }));

            assert_eq!(
                std::fs::read_to_string(&precious).unwrap(),
                "do not delete me"
            );
            assert!(!result.success);
        }

        /// A workspace dir holding content aoe did not put there is kept, not wiped, and the
        /// purge still succeeds.
        #[test]
        fn workspace_dir_with_foreign_content_is_kept_not_failed() {
            let _app_guard = isolate_app_dir();
            for corrupt_ancestor in [true, false] {
                let tmp = tempfile::TempDir::new().unwrap();
                let main_repo = tmp.path().join("frontend");
                init_repo(&main_repo);
                let (workspace, worktree) = if corrupt_ancestor {
                    (tmp.path().to_path_buf(), main_repo.clone())
                } else {
                    let workspace = tmp.path().join("ws");
                    let worktree = workspace.join("frontend");
                    std::fs::create_dir_all(&workspace).unwrap();
                    let path = worktree.to_str().unwrap();
                    git_in(
                        &main_repo,
                        &["worktree", "add", "-b", "feature/ws-del", path, "HEAD"],
                    );
                    (workspace, worktree)
                };
                let stray = workspace.join("stray.txt");
                std::fs::write(&stray, "keep me").unwrap();

                let mut instance = Instance::new("Workspace", workspace.to_str().unwrap());
                let mut repo = workspace_repo(&main_repo, &worktree, "feature/ws-del");
                repo.managed_by_aoe = !corrupt_ancestor;
                instance.workspace_info = Some(workspace_info(&workspace, vec![repo]));
                let result = perform_deletion(&persisted_request(DeletionRequest {
                    delete_worktree: true,
                    delete_branch: !corrupt_ancestor,
                    ..request(instance)
                }));

                assert!(
                    result.success,
                    "a stray file must not wedge the purge: {:?}",
                    result.errors
                );
                assert_eq!(std::fs::read_to_string(&stray).unwrap(), "keep me");
                assert!(workspace.exists());
                assert_eq!(
                    worktree.exists(),
                    corrupt_ancestor,
                    "only a managed worktree goes"
                );
            }
        }

        #[test]
        fn workspace_repo_keeps_a_branch_aoe_did_not_create() {
            let _app_guard = isolate_app_dir();
            let tmp = tempfile::TempDir::new().unwrap();
            let workspace = tmp.path().join("ws");
            let main_repo = tmp.path().join("frontend");
            let worktree = workspace.join("frontend");
            init_repo(&main_repo);
            git_in(&main_repo, &["branch", "mine"]);
            std::fs::create_dir_all(&workspace).unwrap();
            git_in(
                &main_repo,
                &["worktree", "add", worktree.to_str().unwrap(), "mine"],
            );

            let mut instance = Instance::new("Converted", workspace.to_str().unwrap());
            let mut repo = workspace_repo(&main_repo, &worktree, "mine");
            repo.branch_preexisting = true;
            instance.workspace_info = Some(workspace_info(&workspace, vec![repo]));
            let result = perform_deletion(&persisted_request(DeletionRequest {
                delete_worktree: true,
                delete_branch: true,
                ..request(instance)
            }));

            assert!(
                result.success,
                "perform_deletion failed: {:?}",
                result.errors
            );
            assert!(!worktree.exists(), "worktree should be removed");
            assert!(branch_exists(&main_repo, "mine"));
        }

        #[test]
        fn preserved_worktree_keeps_its_branch() {
            let _app_guard = isolate_app_dir();
            let (_tmp, main_repo, worktree_path, instance) = worktree_fixture("feature/keep-me");
            let result = perform_deletion(&persisted_request(DeletionRequest {
                delete_branch: true,
                ..request(instance)
            }));

            assert!(result.success, "{:?}", result.errors);
            assert!(worktree_path.exists());
            assert!(main_repo.join(".git/worktrees/worktree").exists());
            assert!(branch_exists(&main_repo, "feature/keep-me"));
        }

        /// A dirty worktree survives a normal delete (the sandbox preclean is skipped too, or it
        /// would wipe the changes first) and is removed by a forced one.
        #[test]
        fn dirty_worktree_requires_force() {
            let _app_guard = isolate_app_dir();
            for sandboxed in [false, true] {
                let (_tmp, main_repo, worktree_path, mut instance) =
                    worktree_fixture("feature/dirty");
                if sandboxed {
                    instance.sandbox_info = Some(sandbox_info("aoe-dirty-test-doesnotexist"));
                }
                std::fs::write(worktree_path.join("uncommitted.log"), "important").unwrap();
                let request = persisted_request(DeletionRequest {
                    delete_worktree: true,
                    delete_branch: true,
                    delete_sandbox: sandboxed,
                    ..request(instance)
                });
                let result = perform_deletion(&request);
                assert!(!result.success);
                assert_eq!(
                    std::fs::read_to_string(worktree_path.join("uncommitted.log")).unwrap(),
                    "important"
                );
                assert!(branch_exists(&main_repo, "feature/dirty"));
                let result = perform_deletion(&DeletionRequest {
                    force_delete: true,
                    delete_sandbox: false,
                    ..request
                });
                assert!(result.success, "{:?}", result.errors);
                assert!(!worktree_path.exists());
                assert!(!branch_exists(&main_repo, "feature/dirty"));
            }
        }
    }

    mod scratch_cleanup {
        use super::*;
        use std::fs;

        fn scratch_instance() -> (Instance, PathBuf) {
            let id = format!("delete-test-{}", uuid::Uuid::new_v4());
            let dir = crate::session::scratch::provision_scratch_dir(&id)
                .expect("provision scratch dir for test");
            let mut instance = Instance::new("Scratch", dir.to_str().unwrap());
            instance.scratch = true;
            (instance, dir)
        }

        #[test]
        #[serial]
        fn scratch_purge_respects_keep_and_missing_directories() {
            let _tmp = isolate_app_dir();
            for (keep_scratch, missing) in [(true, false), (false, false), (false, true)] {
                let (instance, dir) = scratch_instance();
                if missing {
                    fs::remove_dir_all(&dir).unwrap();
                }
                let request = persisted_request(DeletionRequest {
                    keep_scratch,
                    ..request(instance)
                });
                let result = perform_deletion(&request);
                assert!(
                    result.success,
                    "keep={keep_scratch}, missing={missing}: {:?}",
                    result.errors
                );
                assert_eq!(dir.exists(), keep_scratch && !missing);
                assert!(Storage::open_unwatched("default")
                    .unwrap()
                    .load()
                    .unwrap()
                    .is_empty());
            }
        }

        /// The scratch guard refuses a scratch row pointing outside the scratch root, and a
        /// non-scratch row under the app dir is never treated as scratch.
        #[test]
        #[serial]
        fn scratch_cleanup_leaves_paths_outside_its_root_alone() {
            let _tmp = isolate_app_dir();
            let app_dir = crate::session::get_app_dir().unwrap();
            for (scratch, parent) in [(true, std::env::temp_dir()), (false, app_dir)] {
                let dir = parent.join(format!("aoe-scratch-guard-{}", uuid::Uuid::new_v4()));
                fs::create_dir(&dir).unwrap();
                fs::write(dir.join("file.txt"), b"keep me").unwrap();

                let mut instance = Instance::new("Guarded", dir.to_str().unwrap());
                instance.scratch = scratch;
                let result = perform_deletion(&persisted_request(request(instance)));

                let survived = dir.join("file.txt").exists();
                let _ = fs::remove_dir_all(&dir);
                assert!(survived, "scratch={scratch}: {dir:?} must survive");
                assert_eq!(!result.success, scratch, "{:?}", result.errors);
            }
        }
    }
}
