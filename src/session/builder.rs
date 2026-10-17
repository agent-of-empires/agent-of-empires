//! Instance creation and cleanup utilities.

use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use chrono::Utc;

use crate::containers;
use crate::git::error::GitError;
use crate::git::GitWorktree;

use super::{
    civilizations, Config, Instance, SandboxInfo, WorkspaceInfo, WorkspaceRepo, WorktreeInfo,
};

/// Applies per-session launch values over the config defaults for
/// `instance.tool`. Empty strings and `None` count as unset. Command priority:
/// per-session > `agent_command_override` > `custom_agents` > the value already
/// on `instance`.
pub(crate) fn apply_agent_launch_config(
    instance: &mut Instance,
    session: &super::config::SessionConfig,
    extra_args: &str,
    command_override: &str,
    yolo_mode: Option<bool>,
) {
    let extra = match extra_args {
        "" => session
            .agent_extra_args
            .get(&instance.tool)
            .map_or("", String::as_str),
        set => set,
    };
    if !extra.is_empty() {
        instance.extra_args = extra.to_string();
    }

    let command = match command_override {
        "" => session.resolve_tool_command(&instance.tool),
        set => set.to_string(),
    };
    if !command.is_empty() {
        instance.command = command;
    }

    instance.yolo_mode = yolo_mode.unwrap_or(session.yolo_mode_default);
}

/// Parameters for creating a new session instance.
#[derive(Debug, Clone)]
pub struct InstanceParams {
    pub title: String,
    /// `title` was typed by the user, so the agent may be given it as its own session name.
    pub title_typed: bool,
    pub path: String,
    pub group: String,
    pub tool: String,
    pub worktree_enabled: bool,
    pub worktree_branch: Option<String>,
    pub create_new_branch: bool,
    /// Branch to base a freshly-created worktree branch on.
    pub base_branch: Option<String>,
    pub sandbox: bool,
    /// The sandbox image to use. Required when sandbox is true.
    pub sandbox_image: String,
    pub yolo_mode: bool,
    /// Additional environment entries for the container.
    /// `KEY` = pass through from host, `KEY=VALUE` = set explicitly.
    pub extra_env: Vec<String>,
    /// Extra arguments to append after the agent binary
    pub extra_args: String,
    /// Command override for the agent binary (replaces the default binary)
    pub command_override: String,
    /// Additional repository paths for multi-repo workspace mode
    pub extra_repo_paths: Vec<String>,
    /// Per-repo base branches as `(selector, base)` pairs, from `aoe add --repo-base
    /// <selector>=<ref>` or the web wizard.
    pub repo_base_branches: Vec<(String, String)>,
    /// Scratch session: ignore `path`, provision a fresh directory under `<app_dir>/scratch/<id>/`,
    /// and persist `instance.scratch = true` so the deletion path removes the directory.
    pub scratch: bool,
    /// One-shot fork seed. When `Some`, the freshly-built instance is set up
    /// to fork its parent on first launch instead of starting fresh.
    pub fork_seed: Option<crate::session::ForkSeed>,
    /// Exact originating placeholder, if creation is queued by the TUI.
    pub provisional_creation: Option<std::sync::Arc<ProvisionalCreation>>,
    /// The caller's retained hook plan contains external prepublication callbacks.
    pub has_creation_callbacks: bool,
}

/// Logical identity of the originating durable Creating placeholder.
#[derive(Debug)]
pub struct ProvisionalCreation {
    profile: super::move_journal::ProfileAuthority,
    id: String,
    created_at: chrono::DateTime<Utc>,
    generation: u64,
}

impl ProvisionalCreation {
    pub(crate) fn capture(storage: &super::Storage, row: &Instance) -> Result<Self> {
        anyhow::ensure!(
            row.status == super::Status::Creating
                && row.lifecycle_reservation_is_owned(
                    super::LifecycleOperation::Create,
                    row.lifecycle_generation
                )
                && row
                    .lifecycle_reservation
                    .as_ref()
                    .is_some_and(|lease| lease.resource_intent.is_none()),
            "Creating placeholder has no unbound creation reservation"
        );
        Ok(Self {
            profile: super::move_journal::ProfileAuthority::capture(storage)?,
            id: row.id.clone(),
            created_at: row.created_at,
            generation: row.lifecycle_generation,
        })
    }

    pub(crate) fn instance_id(&self) -> &str {
        &self.id
    }

    pub(crate) fn verify_profile(&self, storage: &super::Storage) -> Result<()> {
        anyhow::ensure!(
            self.profile.matches(storage)?,
            "Creating placeholder profile changed"
        );
        Ok(())
    }

    pub(crate) fn matches_instance(&self, row: &Instance) -> Result<bool> {
        Ok(row.id == self.id
            && row.created_at == self.created_at
            && row.lifecycle_generation == self.generation
            && row.status == super::Status::Creating
            && !row.is_trashed()
            && row
                .lifecycle_reservation_is_owned(super::LifecycleOperation::Create, self.generation))
    }
}

/// Result of building an instance, tracking what was created for cleanup purposes.
pub struct BuildResult {
    pub(crate) profile_storage: super::Storage,
    pub(crate) transaction: Option<std::sync::Arc<super::claim::ResourceReservation>>,
    on_create_effect: Option<usize>,
    on_create_started: bool,
    on_create_completed: bool,
    ready_status: super::Status,
    initial_metadata: Option<Instance>,
    pub instance: Instance,
    /// Path to worktree if one was created and managed by aoe
    pub created_worktree: Option<CreatedWorktree>,
    /// Workspace worktrees created during build (for cleanup)
    pub created_workspace_worktrees: Vec<CreatedWorktree>,
    /// Non-fatal warnings from worktree/workspace creation. Callers should
    /// surface these to the user (post-checkout hook failures etc.).
    pub warnings: Vec<String>,
}

impl BuildResult {
    pub(crate) fn provision(
        profile_storage: super::Storage,
        mut instance: Instance,
        mut effects: Vec<super::move_journal::ResourceEffect>,
        has_creation_callbacks: bool,
        provisional: Option<&ProvisionalCreation>,
        allocations: (Option<CreatedWorktree>, Vec<CreatedWorktree>),
        warnings: Vec<String>,
    ) -> Result<Self> {
        use super::move_journal::{
            CallbackCheckout, IntentOwner, ProfileAuthority, ResourceEffect, ResourceIntent,
        };
        instance.source_profile = profile_storage.profile().to_owned();
        let ready_status = instance.status;
        let (transaction, admitted, on_create_effect) = {
            let root = super::storage::acquire_ownership_lock()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = profile_storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &instance.id)?;
            let before = if let Some(provisional) = provisional {
                provisional.verify_profile(&profile_storage)?;
                let current = profile_storage
                    .load_strict_for_worktree_ownership()?
                    .into_iter()
                    .find(|row| row.id == instance.id)
                    .context("Creating placeholder disappeared before admission")?;
                anyhow::ensure!(
                    provisional.matches_instance(&current)?
                        && current
                            .lifecycle_reservation
                            .as_ref()
                            .is_some_and(|lease| lease.resource_intent.is_none()),
                    "Creating placeholder was superseded"
                );
                Some(current)
            } else {
                None
            };
            let mut reserved = instance.clone();
            if let Some(before) = &before {
                reserved.lifecycle_generation = before.lifecycle_generation;
                reserved.lifecycle_reservation = before.lifecycle_reservation.clone();
                let path = std::mem::take(&mut reserved.project_path);
                let worktree = reserved.worktree_info.take();
                reserved.merge_user_action_diff(&instance, before);
                reserved.project_path = path;
                reserved.worktree_info = worktree;
            } else {
                reserved.try_acquire_lifecycle_reservation(
                    super::LifecycleOperation::Create,
                    Instance::LIFECYCLE_RESERVATION_TTL,
                    Utc::now(),
                )?;
            }
            reserved.status = super::Status::Creating;
            instance.lifecycle_generation = reserved.lifecycle_generation;
            instance.lifecycle_reservation = None;
            if effects.is_empty() && !instance.is_sandboxed() && !has_creation_callbacks {
                validate_creation_paths(&instance)?;
                let admitted = profile_storage.update_with_ownership(&root, |rows, _| {
                    if let Some(current) = rows.iter_mut().find(|row| row.id == reserved.id) {
                        let before = before
                            .as_ref()
                            .context("Creation identity already exists")?;
                        anyhow::ensure!(
                            super::move_journal::row_resources_match(current, before)
                                && current.lifecycle_reservation == before.lifecycle_reservation,
                            "Creating placeholder changed before admission"
                        );
                        *current = reserved;
                        Ok(current.clone())
                    } else {
                        anyhow::ensure!(
                            before.is_none(),
                            "Creating placeholder disappeared before admission"
                        );
                        rows.push(reserved);
                        Ok(rows.last().expect("Creating row inserted").clone())
                    }
                })?;
                (None, admitted, None)
            } else {
                let checkout = |repository: &str, path: &str, branch: &str| {
                    let detached = effects.iter().any(|effect| matches!(effect,
                        ResourceEffect::CreateWorktree { path: planned, expected_detached: true, .. }
                            if planned == Path::new(path)));
                    CallbackCheckout {
                        repository: repository.into(),
                        path: path.into(),
                        branch: (!detached).then(|| branch.to_owned()),
                    }
                };
                let mut checkouts = Vec::new();
                if let Some(workspace) = &instance.workspace_info {
                    checkouts.extend(workspace.repos.iter().map(|repo| {
                        checkout(&repo.main_repo_path, &repo.worktree_path, &repo.branch)
                    }));
                } else if let Some(worktree) = &instance.worktree_info {
                    checkouts.push(checkout(
                        &worktree.main_repo_path,
                        &instance.project_path,
                        &worktree.branch,
                    ));
                }
                let index = effects.len();
                effects.push(ResourceEffect::Callback {
                    name: "creation-callbacks".to_owned(),
                    directory: instance.project_path.clone().into(),
                    checkouts,
                });
                let profile = ProfileAuthority::capture(&profile_storage)?;
                let intent = ResourceIntent::new(
                    IntentOwner {
                        profile: profile.clone(),
                        id: instance.id.clone(),
                        created_at: instance.created_at,
                        generation: reserved.lifecycle_generation,
                    },
                    profile,
                    super::LifecycleOperation::Create,
                    before,
                    Some(instance),
                    effects,
                    Vec::new(),
                )?;
                let admitted = super::claim::ResourceReservation::admit_with_ownership(
                    &profile_storage,
                    &root,
                    intent,
                    reserved,
                )?;
                (Some(admitted.transaction), admitted.instance, Some(index))
            }
        };
        let initial_metadata = transaction.is_none().then(|| admitted.clone());
        let mut result = Self {
            profile_storage,
            transaction,
            on_create_effect,
            on_create_started: false,
            on_create_completed: false,
            ready_status,
            initial_metadata,
            instance: admitted,
            created_worktree: allocations.0,
            created_workspace_worktrees: allocations.1,
            warnings,
        };
        if let Err(error) = result.execute_allocations() {
            return match result.rollback() {
                Ok(()) => Err(error),
                Err(cleanup) => {
                    Err(error.context(format!("Creation rollback could not finish: {cleanup:#}")))
                }
            };
        }
        Ok(result)
    }

    fn execute_allocations(&mut self) -> Result<()> {
        use super::move_journal::ResourceEffect;
        let Some(transaction) = self.transaction.as_ref() else {
            return Ok(());
        };
        let mut jobs = Vec::new();
        for index in 0..self
            .on_create_effect
            .context("Creation callback phase is missing")?
        {
            let effect = transaction.effect(index)?;
            match effect {
                ResourceEffect::CreateWorktree { .. } => jobs.push((index, effect)),
                ResourceEffect::CreateDirectory { .. } => {
                    let scratch = self.instance.scratch;
                    transaction.run_effect(index, || {
                        if scratch {
                            super::scratch::provision_scratch_dir(&self.instance.id)?;
                            Ok((Vec::new(), None))
                        } else {
                            effect.execute().map(|warnings| (warnings, None))
                        }
                    })??;
                }
                _ => bail!("Creation plan contains a non-allocation effect"),
            }
        }
        let outcomes = std::thread::scope(|scope| {
            let handles: Vec<_> = jobs
                .into_iter()
                .map(|(index, effect)| {
                    scope.spawn(move || {
                        let label = match &effect {
                            ResourceEffect::CreateWorktree { repository, .. } => {
                                repository.display().to_string()
                            }
                            _ => unreachable!("only checkout allocations are parallel"),
                        };
                        transaction
                            .run_effect(index, || effect.execute().map(|warnings| (warnings, None)))
                            .and_then(std::convert::identity)
                            .map_err(|error| format!("{label}: {error:#}"))
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| {
                    handle.join().unwrap_or_else(|_| {
                        Err("worktree worker panicked; its Pending intent was retained".to_owned())
                    })
                })
                .collect::<Vec<_>>()
        });
        let mut errors = Vec::new();
        for outcome in outcomes {
            match outcome {
                Ok(warnings) => self.warnings.extend(warnings),
                Err(error) => errors.push(error),
            }
        }
        anyhow::ensure!(
            errors.is_empty(),
            "Failed to create worktrees:\n{}",
            errors.join("\n")
        );
        Ok(())
    }

    fn owns_ordinary_row(&self, row: &Instance) -> bool {
        row.id == self.instance.id
            && row.created_at == self.instance.created_at
            && row.lifecycle_generation == self.instance.lifecycle_generation
            && row.lifecycle_reservation_is_owned(
                super::LifecycleOperation::Create,
                self.instance.lifecycle_generation,
            )
            && row
                .lifecycle_reservation
                .as_ref()
                .is_some_and(|lease| lease.resource_intent.is_none())
            && self
                .initial_metadata
                .as_ref()
                .is_some_and(|before| super::move_journal::same_namespace(before, row))
    }

    fn replace_live(&mut self, mut latest: Instance) {
        let status = latest.status;
        latest.merge_runtime_for_profile_move(&self.instance);
        latest.status = status;
        self.instance = latest;
    }

    fn flush_metadata(&mut self) -> Result<()> {
        let latest = {
            let root = super::storage::acquire_ownership_lock()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = self
                .profile_storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &self.instance.id)?;
            if let Some(transaction) = &self.transaction {
                transaction.update_creation_metadata_with_ownership(&root, &self.instance)?
            } else {
                let before = self
                    .initial_metadata
                    .as_ref()
                    .context("Creation has already been published")?;
                anyhow::ensure!(
                    super::move_journal::same_namespace(before, &self.instance),
                    "Creation initialization changed its namespace"
                );
                self.profile_storage
                    .update_with_ownership(&root, |rows, _| {
                        let current = rows
                            .iter_mut()
                            .find(|row| row.id == self.instance.id)
                            .context("Creating owner disappeared")?;
                        anyhow::ensure!(
                            self.owns_ordinary_row(current),
                            "Creating owner was superseded"
                        );
                        current.merge_creation_diff(before, &self.instance)?;
                        Ok(current.clone())
                    })?
            }
        };
        if self.transaction.is_none() {
            self.initial_metadata = Some(latest.clone());
        }
        self.replace_live(latest);
        Ok(())
    }

    pub(crate) fn run_on_create<T>(
        &mut self,
        callback: impl FnOnce(&mut Instance) -> Result<T>,
    ) -> Result<T> {
        anyhow::ensure!(
            !self.on_create_started,
            "Creation callbacks have already started"
        );
        self.flush_metadata()?;
        self.on_create_started = true;
        let outcome = if let Some(transaction) = &self.transaction {
            transaction.run_effect(
                self.on_create_effect
                    .context("Creation callback phase is missing")?,
                || callback(&mut self.instance).map(|value| (value, None)),
            )?
        } else {
            callback(&mut self.instance)
        };
        match (outcome, self.flush_metadata()) {
            (Ok(value), Ok(())) => {
                self.on_create_completed = true;
                Ok(value)
            }
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(persistence)) => Err(error.context(format!(
                "Creation metadata could not be saved: {persistence:#}"
            ))),
        }
    }

    pub(crate) fn publish(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.on_create_completed,
            "Creation callbacks have not completed"
        );
        self.flush_metadata()?;
        let latest = {
            let root = super::storage::acquire_ownership_lock()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = self
                .profile_storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &self.instance.id)?;
            let validate = |rows: &[Instance], candidate: &Instance| -> Result<()> {
                if super::is_duplicate_session(
                    rows.iter(),
                    &candidate.title,
                    &candidate.project_path,
                    Some(candidate.id.as_str()),
                ) {
                    return Err(super::duplicate_session_error(&candidate.title));
                }
                validate_creation_paths(candidate)
            };
            if let Some(transaction) = &self.transaction {
                transaction
                    .publish_with_ownership(&root, validate)?
                    .context("Creation publication removed its row")?
            } else {
                self.profile_storage
                    .update_with_ownership(&root, |rows, groups| {
                        let index = rows
                            .iter()
                            .position(|row| row.id == self.instance.id)
                            .context("Creating owner disappeared")?;
                        anyhow::ensure!(
                            self.owns_ordinary_row(&rows[index]),
                            "Creating owner was superseded"
                        );
                        rows[index].status = self.ready_status;
                        if rows[index].is_archived() {
                            rows[index].settle_archived_status();
                        }
                        rows[index].lifecycle_reservation = None;
                        validate(rows, &rows[index])?;
                        let committed = rows[index].clone();
                        let mut tree = super::GroupTree::new_with_groups(rows, groups);
                        tree.create_group(&committed.group_path);
                        *groups = tree.get_all_groups();
                        Ok(committed)
                    })?
            }
        };
        self.replace_live(latest);
        self.transaction = None;
        self.initial_metadata = None;
        Ok(())
    }

    pub(crate) fn rollback(&mut self) -> Result<()> {
        if let Some(transaction) = &self.transaction {
            if let Err(error) = transaction.rollback() {
                let diagnostic = {
                    let root = super::storage::acquire_ownership_lock()?;
                    transaction.note_conflict_with_ownership(&root, format!("{error:#}"))
                };
                return match diagnostic {
                    Ok(()) => Err(error),
                    Err(recording) => Err(error.context(format!(
                        "Creation conflict could not be recorded: {recording:#}"
                    ))),
                };
            }
        } else {
            anyhow::ensure!(
                self.initial_metadata.is_some(),
                "Published creation resources were retained"
            );
            let root = super::storage::acquire_ownership_lock()?;
            let _identity = super::storage::acquire_session_identity_lock_with_ownership(&root)?;
            let _lifecycle = self
                .profile_storage
                .acquire_instance_lifecycle_lock_with_ownership(&root, &self.instance.id)?;
            self.profile_storage
                .update_with_ownership(&root, |rows, _| {
                    let index = rows
                        .iter()
                        .position(|row| row.id == self.instance.id)
                        .context("Creating owner disappeared")?;
                    anyhow::ensure!(
                        self.owns_ordinary_row(&rows[index]),
                        "Creating owner was superseded"
                    );
                    rows.remove(index);
                    Ok(())
                })?;
        }
        self.transaction = None;
        self.initial_metadata = None;
        Ok(())
    }
}

/// A worktree provisioned during instance building and owned by this build.
pub struct CreatedWorktree {
    pub path: PathBuf,
    pub main_repo_path: PathBuf,
    /// Branch created by this build. `None` when attaching an existing branch.
    pub owned_branch: Option<String>,
}

/// A complete workspace plan, before any checkout allocation.
pub(crate) struct WorkspacePlan {
    pub(crate) workspace_info: WorkspaceInfo,
    pub(crate) created_worktrees: Vec<CreatedWorktree>,
    pub(crate) workspace_path: PathBuf,
    pub(crate) effects: Vec<super::move_journal::ResourceEffect>,
}

/// Normalize a base-branch string, treating empty/whitespace as unset.
fn normalize_base(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolve a repo's effective base branch with precedence: explicit session base > per-project
/// default > global/profile default.
pub(crate) fn resolve_base_branch(
    session: Option<&str>,
    project: Option<&str>,
    global: Option<&str>,
) -> Option<String> {
    normalize_base(session)
        .or_else(|| normalize_base(project))
        .or_else(|| normalize_base(global))
}

/// Resolve one repo's effective base branch, consulting its registered per-project default.
fn resolve_repo_base_branch(
    repo_path: &std::path::Path,
    session: Option<&str>,
    project_bases: &std::collections::HashMap<String, String>,
    global: Option<&str>,
) -> Option<String> {
    let main_repo =
        GitWorktree::find_main_repo(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
    let key = crate::session::projects::canonical_key(&main_repo.to_string_lossy());
    let project = project_bases.get(&key).map(String::as_str);
    resolve_base_branch(session, project, global)
}

/// Match `(selector, base)` pairs to the repos a session is being built from.
pub(crate) fn resolve_repo_base_selectors(
    repos: &[PathBuf],
    pairs: &[(String, String)],
) -> Result<std::collections::HashMap<PathBuf, String>> {
    let mut out = std::collections::HashMap::new();
    for (selector, base) in pairs {
        let sel = selector.trim();
        let Some(base) = normalize_base(Some(base)) else {
            bail!("No base branch given for repo '{}'", sel);
        };
        let matches: Vec<&PathBuf> = repos
            .iter()
            .filter(|p| {
                p.as_os_str() == sel
                    || p.file_name()
                        .is_some_and(|n| n == std::ffi::OsStr::new(sel))
            })
            .collect();
        match matches.as_slice() {
            [one] => {
                if out.insert((*one).clone(), base).is_some() {
                    bail!("Repo '{}' was given a base branch twice", sel);
                }
            }
            [] => {
                let known: Vec<String> = repos
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string()))
                    .collect();
                bail!(
                    "No repo named '{}' in this session. Available: {}",
                    sel,
                    known.join(", ")
                );
            }
            _ => bail!(
                "Repo name '{}' is ambiguous; pass the full path instead",
                sel
            ),
        }
    }
    Ok(out)
}

/// Map of canonical repo path to configured default base branch for every registered project
/// (global + profile) that sets one.
pub(crate) fn project_base_branches(profile: &str) -> std::collections::HashMap<String, String> {
    crate::session::projects::load_merged(profile)
        .unwrap_or_else(|e| {
            // Don't fork worktrees from the wrong base in silence: if the registry can't be read,
            // log it so the missing per-project defaults are explainable instead of mysterious.
            tracing::warn!(
                target: "session.create",
                "Failed to load project registry for base-branch defaults; \
                 repos fall back to the global default: {e}"
            );
            Vec::new()
        })
        .into_iter()
        .filter_map(|p| {
            let base = p.default_base_branch?;
            let base = base.trim().to_string();
            if base.is_empty() {
                None
            } else {
                Some((crate::session::projects::canonical_key(&p.path), base))
            }
        })
        .collect()
}

/// One repository in a multi-repo workspace, paired with the base branch its freshly-created
/// worktree branch should fork from.
pub struct WorkspaceRepoSpec {
    pub path: PathBuf,
    pub base_branch: Option<String>,
}

/// Plan every workspace checkout before publishing its Creating owner.
pub(crate) fn plan_workspace(
    primary: &WorkspaceRepoSpec,
    extra_repos: &[WorkspaceRepoSpec],
    branch: &str,
    create_new_branch: bool,
    workspace_template: &str,
    init_submodules: bool,
    instance_id: &str,
) -> Result<WorkspacePlan> {
    use super::move_journal::ResourceEffect;
    super::validate_instance_id(instance_id)?;
    let main = GitWorktree::find_main_repo(&primary.path)?;
    let workspace_path = GitWorktree::new(main)?.compute_path(
        branch,
        workspace_template,
        &instance_id[..instance_id.len().min(8)],
    )?;
    let mut effects = vec![ResourceEffect::CreateDirectory {
        path: workspace_path.clone(),
    }];
    let mut repos = Vec::with_capacity(extra_repos.len() + 1);
    let mut created_worktrees = Vec::with_capacity(extra_repos.len() + 1);
    let mut names = HashSet::new();
    for spec in std::iter::once(primary).chain(extra_repos) {
        let path = spec
            .path
            .canonicalize()
            .unwrap_or_else(|_| spec.path.clone());
        if !GitWorktree::is_git_repo(&path) {
            bail!("Path is not in a git repository: {}\nTip: All --repo paths must be git repositories",
                path.display());
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".to_owned());
        anyhow::ensure!(
            names.insert(name.clone()),
            "Duplicate repository name '{name}' in workspace"
        );
        let main = GitWorktree::find_main_repo(&path)?.canonicalize()?;
        let checkout = workspace_path.join(&name);
        anyhow::ensure!(
            !checkout.try_exists()?,
            "Worktree already exists: {}",
            checkout.display()
        );
        effects.push(ResourceEffect::create_worktree(
            main.clone(),
            checkout.clone(),
            branch.to_owned(),
            create_new_branch,
            spec.base_branch.clone(),
            init_submodules,
        )?);
        created_worktrees.push(CreatedWorktree {
            path: checkout.clone(),
            main_repo_path: main.clone(),
            owned_branch: create_new_branch.then(|| branch.to_owned()),
        });
        repos.push(WorkspaceRepo {
            name,
            source_path: path.to_string_lossy().into_owned(),
            branch: branch.to_owned(),
            worktree_path: checkout.to_string_lossy().into_owned(),
            main_repo_path: main.to_string_lossy().into_owned(),
            managed_by_aoe: true,
            branch_preexisting: !create_new_branch,
            base_branch: create_new_branch
                .then(|| spec.base_branch.clone())
                .flatten(),
            base_branch_override: None,
        });
    }
    Ok(WorkspacePlan {
        workspace_info: WorkspaceInfo {
            branch: branch.to_owned(),
            workspace_dir: workspace_path.to_string_lossy().into_owned(),
            repos,
            created_at: Utc::now(),
            cleanup_on_delete: true,
        },
        workspace_path,
        created_worktrees,
        effects,
    })
}

/// Build an instance with all setup (worktree resolution, sandbox config).
pub fn build_instance(
    params: InstanceParams,
    existing_titles: &[&str],
    existing_branches: &[&str],
    profile: &str,
) -> Result<BuildResult> {
    use super::move_journal::ResourceEffect;
    let profile_storage = if params.provisional_creation.is_some() {
        super::Storage::open_unwatched(profile)?
    } else {
        super::Storage::new_unwatched(profile)?
    };
    if let Some(provisional) = &params.provisional_creation {
        provisional.verify_profile(&profile_storage)?;
    }
    // Host-only agents (e.g. settl) cannot run in a sandbox or use worktrees.
    let is_host_only = crate::agents::get_agent(&params.tool).is_some_and(|a| a.host_only);
    if is_host_only && params.sandbox {
        bail!(
            "{} can only run on the host, not in a sandbox.",
            params.tool
        );
    }
    if is_host_only && params.worktree_enabled {
        bail!("{} does not support worktree mode.", params.tool);
    }

    if params.scratch {
        if params.worktree_enabled {
            bail!("Cannot combine --scratch with worktree mode");
        }
        if !params.extra_repo_paths.is_empty() {
            bail!("Cannot combine --scratch with extra repository paths");
        }
    }

    if params.sandbox {
        let runtime = containers::get_container_runtime();
        if !runtime.is_available() {
            bail!("Container runtime is not installed. Please install a supported runtime to use sandbox mode.");
        }
        if !runtime.is_daemon_running() {
            bail!("Container runtime daemon is not running. Please start a supported runtime to use sandbox mode.");
        }
    }

    // Scratch sessions have no project repo, so config resolution falls back to global+profile
    // defaults (`Path::new("")` makes `resolve_config_with_repo` skip the repo-config layer
    // cleanly).
    let config_path = if params.scratch {
        std::path::PathBuf::new()
    } else {
        std::path::PathBuf::from(&params.path)
    };
    let config =
        super::config::repo_config::resolve_config_with_repo(profile, &config_path).unwrap_or_else(|e| {
            tracing::warn!(target: "session.create", "Failed to load config, using defaults: {}", e);
            Config::default()
        });

    let mut final_path = if params.scratch {
        // Provisioning happens after `Instance::new` so we can key the directory on the generated
        // instance id.
        String::new()
    } else {
        PathBuf::from(&params.path)
            .canonicalize()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| params.path.clone())
    };

    let mut worktree_info = None;
    let mut created_worktree = None;
    let mut workspace_info = None;
    let mut created_workspace_worktrees: Vec<CreatedWorktree> = Vec::new();
    let mut effects = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let taken_branches = collect_taken_branches_for_derived_dedupe(
        existing_branches,
        &params.path,
        &params.extra_repo_paths,
        params.worktree_enabled,
        params.create_new_branch,
        params.scratch,
    );
    let final_title = resolve_title(
        &params.title,
        params.worktree_branch.as_deref(),
        params.worktree_enabled,
        existing_titles,
        &taken_branches,
    )?;
    let branch_source = resolve_worktree_branch(
        params.worktree_enabled,
        params.worktree_branch.as_deref(),
        &final_title,
    );

    let effective_worktree_branch: Option<String> = match branch_source {
        None => None,
        Some(BranchSource::Explicit(name)) => Some(name),
        Some(BranchSource::Derived(name)) => {
            if params.create_new_branch {
                Some(dedupe_branch_name(&name, &taken_branches))
            } else {
                Some(name)
            }
        }
    };
    let mut instance = if let Some(provisional) = &params.provisional_creation {
        let current = profile_storage
            .load_strict_for_worktree_ownership()?
            .into_iter()
            .find(|row| row.id == provisional.id)
            .context("Creating placeholder disappeared before planning")?;
        anyhow::ensure!(
            provisional.matches_instance(&current)?,
            "Creating placeholder was superseded"
        );
        current
    } else {
        Instance::new(&final_title, &final_path)
    };
    instance.title.clone_from(&final_title);
    instance.status = super::Status::Idle;
    super::validate_instance_id(&instance.id)?;

    if let Some(branch) = &effective_worktree_branch {
        if !params.extra_repo_paths.is_empty() {
            let primary_path = PathBuf::from(&params.path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&params.path));

            let session_base = params.base_branch.as_deref();
            let global_default = config.worktree.default_base_branch.as_deref();
            let project_bases = project_base_branches(profile);

            // An explicit per-repo base outranks every shared layer, which is the point: one repo
            // forks from develop while the others fork from their own epic branches.
            let mut all_paths = vec![primary_path.clone()];
            all_paths.extend(params.extra_repo_paths.iter().map(PathBuf::from));
            let per_repo = resolve_repo_base_selectors(&all_paths, &params.repo_base_branches)?;
            let base_for = |path: &PathBuf| {
                per_repo.get(path).cloned().or_else(|| {
                    // Every repo, including the launch repo, otherwise forks from its own
                    // registered per-project default when no explicit session base is given.
                    resolve_repo_base_branch(path, session_base, &project_bases, global_default)
                })
            };

            let primary = WorkspaceRepoSpec {
                base_branch: base_for(&primary_path),
                path: primary_path,
            };
            let extra_repos: Vec<WorkspaceRepoSpec> = params
                .extra_repo_paths
                .iter()
                .map(|p| {
                    let path = PathBuf::from(p);
                    WorkspaceRepoSpec {
                        base_branch: base_for(&path),
                        path,
                    }
                })
                .collect();

            let ws_result = plan_workspace(
                &primary,
                &extra_repos,
                branch,
                params.create_new_branch,
                &config.worktree.workspace_path_template,
                config.worktree.init_submodules,
                &instance.id,
            )?;

            final_path = ws_result.workspace_path.to_string_lossy().to_string();
            workspace_info = Some(ws_result.workspace_info);
            created_workspace_worktrees = ws_result.created_worktrees;
            effects.extend(ws_result.effects);
        } else {
            // Single worktree mode (existing logic)
            let path = PathBuf::from(&params.path);
            if !GitWorktree::is_git_repo(&path) {
                // Typed error (not a bare `bail!` string) so the web handler's whitelist forwards
                // an actionable message instead of the opaque "Failed to create session".
                return Err(anyhow::Error::new(GitError::NotAGitRepo).context(format!(
                    "Worktree mode requires a git repository, but this path is not one: {}\n\
                     Tip: start an in-place session (no worktree) here, or point at a git repository.",
                    path.display()
                )));
            }
            let main_repo_path_raw = GitWorktree::find_main_repo(&path)?;
            let main_repo_path = main_repo_path_raw
                .canonicalize()
                .unwrap_or(main_repo_path_raw);
            let git_wt = GitWorktree::new(main_repo_path.clone())?
                .with_init_submodules(config.worktree.init_submodules);

            // Choose appropriate template based on repo type (bare vs regular)
            // Use main_repo_path (not path) to correctly detect bare repos when running from a worktree
            let is_bare = GitWorktree::is_bare_repo(&main_repo_path);
            let template = if is_bare {
                &config.worktree.bare_repo_path_template
            } else {
                &config.worktree.path_template
            };

            if !params.create_new_branch {
                let existing_worktrees = git_wt.list_worktrees()?;
                if let Some(existing) = existing_worktrees
                    .iter()
                    .find(|wt| wt.branch.as_deref() == Some(branch))
                {
                    final_path = existing.path.to_string_lossy().to_string();
                    worktree_info = Some(WorktreeInfo {
                        branch: branch.clone(),
                        main_repo_path: main_repo_path.to_string_lossy().to_string(),
                        managed_by_aoe: false,
                        created_at: Utc::now(),
                        base_branch: None,
                    });
                } else {
                    let worktree_path = git_wt.compute_path(
                        branch,
                        template,
                        &instance.id[..instance.id.len().min(8)],
                    )?;
                    if worktree_path.try_exists()? {
                        return Err(GitError::WorktreeAlreadyExists(worktree_path).into());
                    }
                    effects.push(ResourceEffect::create_worktree(
                        main_repo_path.clone(),
                        worktree_path.clone(),
                        branch.clone(),
                        false,
                        None,
                        config.worktree.init_submodules,
                    )?);

                    final_path = worktree_path.to_string_lossy().to_string();
                    created_worktree = Some(CreatedWorktree {
                        path: worktree_path,
                        main_repo_path: main_repo_path.clone(),
                        owned_branch: None,
                    });
                    worktree_info = Some(WorktreeInfo {
                        branch: branch.clone(),
                        main_repo_path: main_repo_path.to_string_lossy().to_string(),
                        managed_by_aoe: true,
                        created_at: Utc::now(),
                        base_branch: None,
                    });
                }
            } else {
                let worktree_path = git_wt.compute_path(
                    branch,
                    template,
                    &instance.id[..instance.id.len().min(8)],
                )?;

                if worktree_path.exists() {
                    return Err(GitError::WorktreeAlreadyExists(worktree_path.clone()).into());
                }

                // One repo, so a per-repo base can only name this one.
                let per_repo = resolve_repo_base_selectors(
                    std::slice::from_ref(&main_repo_path),
                    &params.repo_base_branches,
                )?;
                // The launch repo otherwise forks from its registered per-project default when no
                // explicit session base is given (then global/profile, then auto-detect).
                let project_bases = project_base_branches(profile);
                let base = per_repo.get(&main_repo_path).cloned().or_else(|| {
                    resolve_repo_base_branch(
                        &main_repo_path,
                        params.base_branch.as_deref(),
                        &project_bases,
                        config.worktree.default_base_branch.as_deref(),
                    )
                });

                effects.push(ResourceEffect::create_worktree(
                    main_repo_path.clone(),
                    worktree_path.clone(),
                    branch.clone(),
                    true,
                    base.clone(),
                    config.worktree.init_submodules,
                )?);

                final_path = worktree_path.to_string_lossy().to_string();
                created_worktree = Some(CreatedWorktree {
                    path: worktree_path,
                    main_repo_path: main_repo_path.clone(),
                    owned_branch: Some(branch.clone()),
                });
                worktree_info = Some(WorktreeInfo {
                    branch: branch.clone(),
                    main_repo_path: main_repo_path.to_string_lossy().to_string(),
                    managed_by_aoe: true,
                    created_at: Utc::now(),
                    base_branch: base,
                });
            }
        }
    }

    if !params.scratch && effects.is_empty() {
        let final_path_buf = PathBuf::from(&final_path);
        if !final_path_buf.exists() {
            bail!("Project path does not exist: {}", final_path);
        }
        if !final_path_buf.is_dir() {
            bail!("Project path is not a directory: {}", final_path);
        }
    }

    instance.project_path = final_path;
    instance.first_launch_names_agent = params.title_typed;
    if params.scratch {
        let dir = super::scratch::scratch_root()?.join(&instance.id);
        anyhow::ensure!(
            !dir.try_exists()?,
            "Scratch directory already exists: {}",
            dir.display()
        );
        effects.push(ResourceEffect::CreateDirectory { path: dir.clone() });
        instance.project_path = dir.to_string_lossy().to_string();
        instance.scratch = true;
    }
    instance.group_path = params.group;
    instance.tool = params.tool.clone();
    instance.detect_as = config
        .session
        .agent_detect_as
        .get(&params.tool)
        .cloned()
        .unwrap_or_default();
    instance.command = crate::agents::get_agent(&params.tool)
        .filter(|a| a.set_default_command)
        .map(|a| a.binary.to_string())
        .unwrap_or_default();
    if let Some(notice) =
        crate::agents::get_agent(&params.tool).and_then(crate::agents::AgentDef::lifecycle_notice)
    {
        // Non-blocking: deprecated agents still launch; every support path
        // is unchanged. The warning only informs.
        tracing::warn!(target: "session.builder", "agent '{}' is {notice}", params.tool);
    }
    instance.worktree_info = worktree_info;
    instance.workspace_info = workspace_info;
    apply_agent_launch_config(
        &mut instance,
        &config.session,
        &params.extra_args,
        &params.command_override,
        Some(params.yolo_mode),
    );
    if instance.command.trim().is_empty() && crate::agents::get_agent(&params.tool).is_none() {
        bail!(
            "No launch command resolved for custom agent '{}'. Config may have changed since validation.",
            params.tool
        );
    }

    if params.sandbox {
        // Surface env-resolution warnings up-front.
        let effective_env: &[String] = if params.extra_env.is_empty() {
            &config.sandbox.environment
        } else {
            &params.extra_env
        };
        warnings.extend(crate::session::validate_env_entries(effective_env));

        instance.sandbox_info = Some(SandboxInfo {
            enabled: true,
            container_id: None,
            image: params.sandbox_image.clone(),
            container_name: containers::DockerContainer::generate_name(&instance.id),
            extra_env: if params.extra_env.is_empty() {
                None
            } else {
                Some(params.extra_env.clone())
            },
            custom_instruction: config.sandbox.custom_instruction.clone(),
            before_start_env: Vec::new(),
            provider: None,
            container_workdir: None,
        });
    }

    if let Some(seed) = params.fork_seed {
        match seed {
            crate::session::ForkSeed::Terminal {
                parent,
                child_session_id,
                unattributed_parent_agent,
            } => {
                // Only an unattributed parent needs this: the launch
                // identity-checks a qualified one itself. The parent's
                // capability came from its own row's agent, so the child must
                // actually launch that same agent.
                if let Some(parent_agent) = unattributed_parent_agent.as_deref() {
                    let launched = Instance::execution_agent_for(
                        &instance.tool,
                        instance.get_tool_command(),
                        &config.session,
                    )
                    .map_err(anyhow::Error::msg)?;
                    crate::session::fork::ensure_child_matches_parent_agent(
                        Some(parent_agent),
                        launched.name,
                    )
                    .map_err(anyhow::Error::msg)?;
                }
                instance.agent_session_id = Some(child_session_id);
                instance.resume_intent = crate::session::ResumeIntent::Fork {
                    from: parent.session_id.clone(),
                };
                instance.resume_binding = Some(*parent);
            }
            crate::session::ForkSeed::Structured {
                parent_acp_session_id,
            } => {
                // Structured fork: force the structured view, seed the parent for the ACP
                // session/fork handshake, and replay history into the (empty) event store on first
                // connect.
                instance.view = crate::session::View::Structured;
                instance.fork_pending = Some(parent_acp_session_id);
                instance.import_pending = Some(true);
            }
        }
    }

    BuildResult::provision(
        profile_storage,
        instance,
        effects,
        params.has_creation_callbacks,
        params.provisional_creation.as_deref(),
        (created_worktree, created_workspace_worktrees),
        warnings,
    )
}

fn validate_creation_paths(instance: &Instance) -> Result<()> {
    anyhow::ensure!(
        Path::new(&instance.project_path).is_dir(),
        "Session working directory disappeared before publication"
    );
    if let Some(workspace) = &instance.workspace_info {
        anyhow::ensure!(
            Path::new(&workspace.workspace_dir).is_dir(),
            "Session workspace disappeared before publication"
        );
        for repo in &workspace.repos {
            validate_checkout(
                &repo.main_repo_path,
                &repo.worktree_path,
                &repo.branch,
                repo.managed_by_aoe,
            )?;
        }
    } else if let Some(worktree) = &instance.worktree_info {
        validate_checkout(
            &worktree.main_repo_path,
            &instance.project_path,
            &worktree.branch,
            worktree.managed_by_aoe,
        )?;
    }
    Ok(())
}

fn validate_checkout(main_repo: &str, path: &str, branch: &str, allocated: bool) -> Result<()> {
    let path = Path::new(path).canonicalize()?;
    let repo = crate::git::open_repo_at(&path)?;
    let main = crate::git::open_repo_at(Path::new(main_repo))?;
    anyhow::ensure!(
        repo.commondir().canonicalize()? == main.commondir().canonicalize()?,
        "Session checkout belongs to another repository"
    );
    let detached = repo.head_detached()?;
    if detached {
        anyhow::ensure!(
            allocated,
            "Reused checkout no longer uses its requested branch"
        );
    } else {
        anyhow::ensure!(
            repo.find_reference("HEAD")?
                .symbolic_target()?
                .and_then(|name| name.strip_prefix("refs/heads/"))
                == Some(branch),
            "Session checkout branch changed before publication"
        );
    }
    anyhow::ensure!(
        GitWorktree::new(PathBuf::from(main_repo))?
            .list_worktrees()?
            .iter()
            .any(
                |entry| entry.path.canonicalize().ok().as_ref() == Some(&path)
                    && entry.is_detached == detached
            ),
        "Session checkout registration changed before publication"
    );
    Ok(())
}

/// Structured-view (ACP) helpers for the TUI create paths.
pub mod structured {
    use super::Instance;

    /// True when `tool` can back a structured-view session: it resolves in the ACP agent registry,
    /// the resolved config declares a parsable `[session.agent_acp_cmd]` command for it, or it is a
    /// custom agent that inherits a registry-backed base through `[session.agent_detect_as]` (e.g.
    /// a Claude wrapper that only overrides profile/oauth locations).
    pub fn tool_acp_capable(tool: &str, config: &crate::session::Config) -> bool {
        crate::acp::agent_registry::AgentRegistry::with_defaults()
            .get(tool)
            .is_some()
            || config
                .session
                .agent_acp_cmd
                .get(tool)
                .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(tool, cmd).is_ok())
            || crate::acp::inherited_acp_base(tool, &config.session.agent_detect_as).is_some()
    }

    /// Pre-create validation for an explicit structured-view choice from the new-session wizard,
    /// run BEFORE any worktree / scratch / container is provisioned so a refusal can't orphan
    /// resources (same ordering as the CLI's precondition).
    pub fn validate_structured_choice(
        tool: &str,
        command_override: &str,
        config: &crate::session::Config,
    ) -> Result<(), String> {
        if !tool_acp_capable(tool, config) {
            return Err(format!(
                "tool `{tool}` is not ACP-capable: it has no agent registry entry and no \
                 [session.agent_acp_cmd] command. Run `aoe acp doctor` to see configured \
                 agents, or turn Structured off for a terminal session."
            ));
        }
        if !command_override.trim().is_empty() {
            return Ok(());
        }
        let registry = crate::acp::agent_registry::AgentRegistry::with_defaults();
        let spec = match registry.get(tool) {
            Some(spec) => spec.clone(),
            None => match config.session.agent_acp_cmd.get(tool) {
                Some(cmd) => crate::acp::AgentSpec::from_acp_cmd(tool, cmd)
                    .map_err(|e| format!("invalid [session.agent_acp_cmd] for `{tool}`: {e}"))?,
                // A custom agent that inherits a registry-backed base runs the
                // base agent's adapter, so the on-PATH check targets that.
                None => match crate::acp::inherited_acp_base(tool, &config.session.agent_detect_as)
                    .and_then(|base| registry.get(&base).cloned())
                {
                    Some(spec) => spec,
                    None => unreachable!("tool_acp_capable implies a resolvable spec"),
                },
            },
        };
        if !crate::cli::acp::command_present(&spec.command) {
            let hint = crate::acp::install_hints::install_hint_for(&spec.command)
                .unwrap_or("install via your package manager and retry");
            return Err(format!(
                "ACP adapter `{}` is not installed or not on $PATH. Install: {hint}. \
                 Or run `aoe acp doctor --fix`, or turn Structured off for a terminal session.",
                spec.command
            ));
        }
        Ok(())
    }

    /// Apply a validated structured-view choice to a freshly-built instance: set the persisted view
    /// and pin the per-agent default model, the same post-build step the web create handler runs.
    pub fn apply_structured_choice(instance: &mut Instance) {
        let config = crate::session::config::repo_config::resolve_config_with_repo_or_warn(
            &instance.source_profile,
            std::path::Path::new(&instance.project_path),
        );
        if !tool_acp_capable(&instance.tool, &config) {
            tracing::warn!(
                target: "session.create",
                session = %instance.id,
                tool = %instance.tool,
                "structured view requested for non-ACP tool; keeping terminal view"
            );
            return;
        }
        instance.view = crate::session::View::Structured;
        // Pin the per-agent default model so the composer shows it and the session stays on it
        // (mirrors the CLI and web create paths).
        let defaults = config.acp.acp_defaults_for(&instance.tool);
        instance.agent_model = crate::session::config::resolve_spawn_model_effort(
            defaults,
            instance.agent_model.take(),
            None,
        )
        .0;
    }
}

/// Resolve the session title: use the provided title, then an explicit worktree
/// branch name, then fall back to a random civilization name.
pub(crate) fn resolve_title(
    title: &str,
    worktree_branch: Option<&str>,
    worktree_enabled: bool,
    existing_titles: &[&str],
    taken_branches: &HashSet<String>,
) -> Result<String> {
    let taken_branch_keys = branch_collision_keys(taken_branches);
    let resolved = if title.is_empty() {
        if worktree_enabled {
            if let Some(branch) = worktree_branch.filter(|b| !b.trim().is_empty()) {
                branch.trim().to_string()
            } else {
                civilizations::generate_random_title_filtered(existing_titles, |candidate| {
                    branch_key_taken(&branch_name_from_title(candidate), &taken_branch_keys)
                })
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Could not generate a unique worktree title or branch; please enter one manually."
                    )
                })?
            }
        } else {
            civilizations::generate_random_title(existing_titles)
        }
    } else {
        title.to_string()
    };

    Ok(resolved)
}

pub(crate) fn collect_taken_branches_for_derived_dedupe(
    existing_branches: &[&str],
    path: &str,
    extra_repo_paths: &[String],
    worktree_enabled: bool,
    create_new_branch: bool,
    scratch: bool,
) -> HashSet<String> {
    let mut taken: HashSet<String> = existing_branches.iter().map(|s| (*s).to_string()).collect();

    if worktree_enabled && create_new_branch && !scratch {
        for repo in std::iter::once(path)
            .chain(extra_repo_paths.iter().map(String::as_str))
            .filter(|s| !s.trim().is_empty())
        {
            if let Ok(local) = crate::git::diff::list_branches(std::path::Path::new(repo)) {
                taken.extend(local);
            }
        }
    }

    taken
}

/// Origin of an effective worktree branch name.
#[derive(Debug, Clone)]
pub(crate) enum BranchSource {
    /// User typed this name explicitly. Treat conflicts as a hard error.
    Explicit(String),
    /// Derived from the session title. Suffix on conflict.
    Derived(String),
}

fn resolve_worktree_branch(
    worktree_enabled: bool,
    worktree_branch: Option<&str>,
    final_title: &str,
) -> Option<BranchSource> {
    if !worktree_enabled {
        return None;
    }
    Some(
        match worktree_branch.map(str::trim).filter(|b| !b.is_empty()) {
            // Defense-in-depth: even if the frontend slug missed a forbidden char (or the caller is
            // a CLI/API user typing a title-shaped string into the branch field), sanitise here so
            // libgit2 never sees a value it'll reject with InvalidSpec.
            Some(b) => BranchSource::Explicit(git_sanitize_branch_name(b)),
            None => BranchSource::Derived(branch_name_from_title(final_title)),
        },
    )
}

/// Replace characters that git ref names cannot contain (per `git-check-ref-format(1)`) with '-'.
pub(crate) fn git_sanitize_branch_name(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_was_dash = false;
    for ch in s.trim().chars() {
        let forbidden = ch.is_whitespace()
            || ch.is_control()
            || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\');
        let push_ch = if forbidden { '-' } else { ch };
        if push_ch == '-' {
            if out.is_empty() || last_was_dash {
                continue;
            }
            last_was_dash = true;
        } else {
            last_was_dash = false;
        }
        out.push(push_ch);
    }
    // Disallowed multi-char sequences: ".." and "@{".
    let mut out = out.replace("..", "-").replace("@{", "-");
    // Strip the ".lock" suffix from every slash-separated component, not just the last one;
    // git-check-ref-format(1) rejects any component ending in ".lock" (e.g. `foo.lock/bar` is just
    // as invalid as `foo.lock`).
    out = out
        .split('/')
        .map(|mut seg| {
            while let Some(stripped) = seg.strip_suffix(".lock") {
                seg = stripped;
            }
            seg
        })
        .collect::<Vec<_>>()
        .join("/");
    while matches!(out.chars().last(), Some('-' | '.' | '/')) {
        out.pop();
    }
    while matches!(out.chars().next(), Some('-' | '.' | '/')) {
        out.remove(0);
    }
    // A lone '@' and the symbolic ref HEAD are also rejected by git as
    // complete ref names.
    if out.is_empty() || out == "@" || out == "HEAD" {
        "session".to_string()
    } else {
        out
    }
}

/// Find the next branch name not present in `taken`.
fn branch_collision_key(branch: &str) -> String {
    branch.to_ascii_lowercase()
}

fn branch_collision_keys(taken: &HashSet<String>) -> HashSet<String> {
    taken
        .iter()
        .map(|branch| branch_collision_key(branch))
        .collect()
}

fn branch_key_taken(branch: &str, taken_keys: &HashSet<String>) -> bool {
    taken_keys.contains(&branch_collision_key(branch))
}

fn dedupe_branch_name(base: &str, taken: &HashSet<String>) -> String {
    let taken_keys = branch_collision_keys(taken);
    if !branch_key_taken(base, &taken_keys) {
        return base.to_string();
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{}-{}", base, n);
        if !branch_key_taken(&candidate, &taken_keys) {
            return candidate;
        }
        n += 1;
    }
}

/// Map Latin ligatures and stroked letters to their conventional ASCII expansions.
fn expand_ligature(c: char) -> Option<&'static str> {
    Some(match c {
        'ß' => "ss",
        'æ' => "ae",
        'Æ' => "AE",
        'œ' => "oe",
        'Œ' => "OE",
        'ø' => "o",
        'Ø' => "O",
        'ł' => "l",
        'Ł' => "L",
        'đ' => "d",
        'Đ' => "D",
        'þ' => "th",
        'Þ' => "Th",
        _ => return None,
    })
}

pub(crate) fn branch_name_from_title(title: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let mut branch = String::new();
    let mut last_was_dash = false;

    let mut push_processed = |ch: char| {
        // Preserve '/' as git's namespace separator (so a title like `jacob/feature-1` yields a
        // branch `jacob/feature-1`).
        if ch == '/' {
            while branch.ends_with('-') {
                branch.pop();
            }
            if branch.is_empty() || branch.ends_with('/') {
                return;
            }
            branch.push('/');
            last_was_dash = true;
            return;
        }

        let next = if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            Some(ch.to_ascii_lowercase())
        } else if ch.is_whitespace() || ch.is_ascii_punctuation() {
            Some('-')
        } else {
            None
        };

        if let Some(ch) = next {
            if ch == '-' {
                if branch.is_empty() || last_was_dash {
                    return;
                }
                last_was_dash = true;
            } else {
                last_was_dash = false;
            }
            branch.push(ch);
        }
    };

    for ch in title.trim().nfkd() {
        match expand_ligature(ch) {
            Some(expansion) => expansion.chars().for_each(&mut push_processed),
            None => push_processed(ch),
        }
    }

    while branch.ends_with('-') || branch.ends_with('/') {
        branch.pop();
    }

    if branch.is_empty() {
        "session".to_string()
    } else {
        branch
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn provision_worktree_for_test(main: &Path, path: &Path, branch: &str) -> Result<BuildResult> {
        let mut instance = Instance::new("worktree-test", &path.to_string_lossy());
        instance.worktree_info = Some(WorktreeInfo {
            branch: branch.to_owned(),
            main_repo_path: main.to_string_lossy().into_owned(),
            managed_by_aoe: true,
            created_at: Utc::now(),
            base_branch: Some("release".into()),
        });
        let effect = super::super::move_journal::ResourceEffect::create_worktree(
            main.to_path_buf(),
            path.to_path_buf(),
            branch.to_owned(),
            true,
            Some("release".into()),
            false,
        )?;
        BuildResult::provision(
            super::super::Storage::new_unwatched("default")?,
            instance,
            vec![effect],
            false,
            None,
            (
                Some(CreatedWorktree {
                    path: path.to_path_buf(),
                    main_repo_path: main.to_path_buf(),
                    owned_branch: Some(branch.to_owned()),
                }),
                Vec::new(),
            ),
            Vec::new(),
        )
    }

    fn provision_workspace_for_test(
        primary: &WorkspaceRepoSpec,
        extra: &[WorkspaceRepoSpec],
        branch: &str,
        create_branch: bool,
        template: &str,
        init_submodules: bool,
    ) -> Result<BuildResult> {
        let mut instance = Instance::new("workspace-test", "");
        let WorkspacePlan {
            workspace_info,
            created_worktrees,
            workspace_path,
            effects,
        } = plan_workspace(
            primary,
            extra,
            branch,
            create_branch,
            template,
            init_submodules,
            &instance.id,
        )?;
        instance.project_path = workspace_path.to_string_lossy().into_owned();
        instance.workspace_info = Some(workspace_info);
        BuildResult::provision(
            super::super::Storage::new_unwatched("default")?,
            instance,
            effects,
            false,
            None,
            (None, created_worktrees),
            Vec::new(),
        )
    }

    #[test]
    #[serial_test::serial]
    fn creation_publication_rejects_removed_checkout() {
        let _home = super::super::test_support::isolate_app_dir();
        let (parent, _) = init_repo_with_branch("publication", "release");
        let main = parent.path().join("publication");
        let path = parent.path().join("checkout");
        let mut result = provision_worktree_for_test(&main, &path, "publication-branch").unwrap();
        result.run_on_create(|_| Ok(())).unwrap();
        let git = GitWorktree::new(main).unwrap();
        git.remove_worktree(&path, false).unwrap();
        assert!(result.publish().is_err());
        assert!(result.rollback().is_err());
        assert!(!path.exists());
        assert!(git.branch_exists("publication-branch").unwrap());
        git.delete_branch("publication-branch").unwrap();
    }

    #[test]
    #[serial_test::serial]
    fn creation_reservation_blocks_cross_profile_checkout_adoption() {
        let _home = super::super::test_support::isolate_app_dir();
        let (parent, _) = init_repo_with_branch("creation-owner", "release");
        let main = parent.path().join("creation-owner");
        let path = parent.path().join("checkout");
        let mut result = provision_worktree_for_test(&main, &path, "claimed-branch").unwrap();
        let peer = super::super::Storage::new_unwatched("later-owner").unwrap();
        assert!(peer
            .update(|rows, _| {
                rows.push(Instance::new("peer", &path.to_string_lossy()));
                Ok(())
            })
            .is_err());
        result.rollback().unwrap();
        assert!(!path.exists());
        assert!(!GitWorktree::new(main)
            .unwrap()
            .branch_exists("claimed-branch")
            .unwrap());
    }

    #[test]
    #[serial_test::serial]
    fn creation_reservation_distinguishes_point_and_recursive_claims() {
        for claim in ["ancestor", "aliased-ancestor", "child", "workspace"] {
            let app = tempfile::tempdir().unwrap();
            let _home = super::super::test_support::isolate_app_dir_at(app.path());
            let (parent, _) = init_repo_with_branch("rollback-claims", "release");
            let main = parent.path().join("rollback-claims");
            let path = parent.path().join("checkout");
            let reference = match claim {
                "ancestor" | "workspace" => parent.path().to_path_buf(),
                "aliased-ancestor" => {
                    let alias = parent.path().join("alias");
                    std::os::unix::fs::symlink(parent.path(), &alias).unwrap();
                    alias
                }
                "child" => path.join("nested"),
                _ => unreachable!(),
            };
            let peer = super::super::Storage::new_unwatched("raw-peer").unwrap();
            peer.update(|rows, _| {
                let mut row = Instance::new("raw", &reference.to_string_lossy());
                if claim == "workspace" {
                    row.workspace_info = Some(super::super::WorkspaceInfo {
                        branch: "main".into(),
                        workspace_dir: reference.to_string_lossy().into_owned(),
                        repos: Vec::new(),
                        created_at: Utc::now(),
                        cleanup_on_delete: true,
                    });
                }
                rows.push(row);
                Ok(())
            })
            .unwrap();
            let result = provision_worktree_for_test(&main, &path, "claimed-branch");
            if matches!(claim, "ancestor" | "aliased-ancestor") {
                let mut result = result.unwrap();
                assert!(path.is_dir());
                result.rollback().unwrap();
            } else {
                assert!(result.is_err(), "{claim}: overlapping claim admitted");
            }
            assert!(
                !path.exists(),
                "{claim}: checkout survived rollback/refusal"
            );
            assert!(!GitWorktree::new(main)
                .unwrap()
                .branch_exists("claimed-branch")
                .unwrap());
        }
    }

    fn roman_for_test(n: u32) -> String {
        let mut remaining = n;
        let mut result = String::new();
        for (value, numeral) in [
            (1000, "M"),
            (900, "CM"),
            (500, "D"),
            (400, "CD"),
            (100, "C"),
            (90, "XC"),
            (50, "L"),
            (40, "XL"),
            (10, "X"),
            (9, "IX"),
            (5, "V"),
            (4, "IV"),
            (1, "I"),
        ] {
            while remaining >= value {
                result.push_str(numeral);
                remaining -= value;
            }
        }
        result
    }

    #[test]
    fn resolve_title_prefers_explicit_then_branch_then_civilization() {
        let taken = HashSet::new();
        assert_eq!(
            resolve_title("My Session", Some("feature-auth"), true, &[], &taken).unwrap(),
            "My Session"
        );
        assert_eq!(
            resolve_title("Custom Name", None, false, &[], &taken).unwrap(),
            "Custom Name"
        );
        assert_eq!(
            resolve_title("", Some("feature-auth"), true, &[], &taken).unwrap(),
            "feature-auth"
        );
        let generated = resolve_title("", None, false, &[], &taken).unwrap();
        assert!(
            civilizations::CIVILIZATIONS.contains(&generated.as_str()),
            "expected a civilization name, got: {generated}"
        );

        let existing: Vec<&str> = civilizations::CIVILIZATIONS
            .iter()
            .copied()
            .filter(|civ| *civ != "Tatars")
            .collect();
        let mut taken = HashSet::new();
        taken.insert("tatars".to_string());

        let title = resolve_title("", None, true, &existing, &taken).unwrap();

        assert_ne!(title, "Tatars");
        assert!(
            title.contains(" II"),
            "expected suffixed fallback after the only bare civ branch was taken, got: {title}"
        );
    }

    #[test]
    fn test_empty_worktree_title_errors_when_generation_exhausts() {
        let existing: Vec<&str> = civilizations::CIVILIZATIONS.to_vec();
        let mut taken = HashSet::new();

        for civ in civilizations::CIVILIZATIONS {
            for n in 2..=1000 {
                taken.insert(branch_name_from_title(&format!(
                    "{} {}",
                    civ,
                    roman_for_test(n)
                )));
            }
        }

        let timestamp = chrono::Utc::now().timestamp();
        for civ in civilizations::CIVILIZATIONS {
            for n in timestamp - 60..timestamp + 1060 {
                taken.insert(branch_name_from_title(&format!("{} {}", civ, n)));
            }
        }

        let err = resolve_title("", None, true, &existing, &taken).unwrap_err();

        assert!(
            err.to_string().contains("please enter one manually"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn resolve_worktree_branch_cases() {
        let branch = |name: Option<&str>| resolve_worktree_branch(true, name, "Fix Login Flow");
        assert!(matches!(branch(None), Some(BranchSource::Derived(s)) if s == "fix-login-flow"));
        assert!(
            matches!(branch(Some("feat/auth")), Some(BranchSource::Explicit(s)) if s == "feat/auth")
        );
        assert!(
            matches!(branch(Some("Exploration and issues v2")), Some(BranchSource::Explicit(s)) if s == "Exploration-and-issues-v2")
        );
        assert!(
            resolve_worktree_branch(false, Some("feat/auth"), "Fix Login Flow").is_none(),
            "no worktree means no branch to resolve"
        );
    }

    #[test]
    fn git_sanitize_branch_name_cases() {
        for (input, want) in [
            // Valid refs pass through untouched.
            ("feat/auth", "feat/auth"),
            ("release-1.2.3", "release-1.2.3"),
            ("user_name/topic", "user_name/topic"),
            // Characters git forbids in a ref.
            ("has spaces", "has-spaces"),
            ("a:b?c*d", "a-b-c-d"),
            ("ref^name", "ref-name"),
            ("a..b", "a-b"),
            ("a@{b", "a-b"),
            // Trimmed edges.
            ("  hello  ", "hello"),
            ("-leading", "leading"),
            (".hidden", "hidden"),
            ("/foo", "foo"),
            ("foo/", "foo"),
            // `.lock` is stripped per component, however many are stacked.
            ("foo.lock", "foo"),
            ("foo.lock/bar", "foo/bar"),
            ("feat/release.lock/v2", "feat/release/v2"),
            ("foo.lock.lock", "foo"),
            ("feat/release.lock.lock/v2.lock.lock", "feat/release/v2"),
            // Nothing usable, or a ref with a reserved meaning of its own.
            ("", "session"),
            ("@", "session"),
            ("HEAD", "session"),
        ] {
            assert_eq!(git_sanitize_branch_name(input), want, "input {input:?}");
        }
    }

    #[test]
    fn branch_name_from_title_cases() {
        for (title, want) in [
            // Git-hostile punctuation.
            ("Fix: login @ mobile #42", "fix-login-mobile-42"),
            ("feat/auth.refactor", "feat/auth-refactor"),
            // Slashes are kept as path separators but never doubled or dangling.
            ("jacob/feature-1", "jacob/feature-1"),
            ("/leading", "leading"),
            ("trailing/", "trailing"),
            ("a//b", "a/b"),
            ("a / b", "a/b"),
            // Latin diacritics and ligatures fold to ASCII.
            ("café fix", "cafe-fix"),
            ("naïve solution", "naive-solution"),
            ("Straße", "strasse"),
            ("Łódź", "lodz"),
            ("crème brûlée", "creme-brulee"),
            ("œuvre", "oeuvre"),
            // Scripts with no ASCII folding drop out.
            ("测试", "session"),
            ("🚀 ship", "ship"),
        ] {
            assert_eq!(branch_name_from_title(title), want, "title {title:?}");
        }
    }

    #[test]
    fn dedupe_branch_name_suffixes_past_every_taken_name() {
        let mut taken = HashSet::new();
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug");

        taken.insert("fix-bug".to_string());
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-2");

        taken.extend(["fix-bug-2".to_string(), "fix-bug-3".to_string()]);
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-4");

        taken.insert("Tatars".to_string());
        assert_eq!(
            dedupe_branch_name("tatars", &taken),
            "tatars-2",
            "collisions are case-insensitive"
        );
    }

    #[test]
    fn resolve_base_branch_precedence() {
        assert_eq!(
            resolve_base_branch(Some("session"), Some("project"), Some("global")),
            Some("session".to_string())
        );
        assert_eq!(
            resolve_base_branch(None, Some("project"), Some("global")),
            Some("project".to_string())
        );
        assert_eq!(
            resolve_base_branch(None, None, Some("global")),
            Some("global".to_string())
        );
        assert_eq!(resolve_base_branch(None, None, None), None);
        assert_eq!(
            resolve_base_branch(Some("   "), Some(""), Some("global")),
            Some("global".to_string())
        );
        assert_eq!(resolve_base_branch(Some("  "), None, None), None);
    }

    #[test]
    fn resolve_repo_base_branch_keys_launch_repo_by_root() {
        let (parent, _tip) = init_repo_with_branch("proj", "release");
        let root = parent.path().join("proj");
        let key = crate::session::projects::canonical_key(&root.to_string_lossy());
        let mut bases = std::collections::HashMap::new();
        bases.insert(key, "develop".to_string());

        assert_eq!(
            resolve_repo_base_branch(&root, None, &bases, Some("global")),
            Some("develop".to_string())
        );

        assert_eq!(
            resolve_repo_base_branch(&root, Some("hotfix"), &bases, Some("global")),
            Some("hotfix".to_string())
        );

        let empty = std::collections::HashMap::new();
        assert_eq!(
            resolve_repo_base_branch(&root, None, &empty, Some("global")),
            Some("global".to_string())
        );

        // Launching from a linked worktree still keys by the main repo root.
        let wt_path = parent.path().join("proj-wt");
        GitWorktree::new(root.clone())
            .unwrap()
            .create_worktree("wt-branch", &wt_path, true, None)
            .unwrap();
        assert_eq!(
            resolve_repo_base_branch(&wt_path, None, &bases, None),
            Some("develop".to_string())
        );
    }

    fn init_repo_with_branch(name: &str, branch: &str) -> (tempfile::TempDir, git2::Oid) {
        let parent = tempfile::Builder::new()
            .prefix("aoe-test-")
            .tempdir()
            .unwrap();
        let dir = parent.path().join(name);
        std::fs::create_dir(&dir).unwrap();
        let repo = git2::Repository::init(&dir).unwrap();
        let sig = git2::Signature::now("Test", "test@example.com").unwrap();

        std::fs::write(dir.join("README.md"), format!("{name}\n")).unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("README.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let base_commit = repo
            .commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();

        let base = repo.find_commit(base_commit).unwrap();
        repo.branch(branch, &base, false).unwrap();
        std::fs::write(dir.join("RELEASE.md"), "release\n").unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            index.add_path(std::path::Path::new("RELEASE.md")).unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        let branch_ref = format!("refs/heads/{branch}");
        let release_commit = repo
            .commit(Some(&branch_ref), &sig, &sig, "release", &tree, &[&base])
            .unwrap();

        (parent, release_commit)
    }

    #[test]
    fn create_workspace_honors_per_repo_base_branch() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (parent_primary, _) = init_repo_with_branch("primary", "release");
        let (parent_extra, extra_release_tip) = init_repo_with_branch("extra", "release");
        let primary = parent_primary.path().join("primary");
        let extra = parent_extra.path().join("extra");
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();

        let mut result = provision_workspace_for_test(
            &WorkspaceRepoSpec {
                path: primary,
                base_branch: None,
            },
            &[WorkspaceRepoSpec {
                path: extra,
                base_branch: Some("release".to_string()),
            }],
            "feature-x",
            true,
            &template,
            true,
        )
        .expect("workspace creation should succeed");

        let workspace = result.instance.workspace_info.as_ref().unwrap();
        let extra_repo = workspace
            .repos
            .iter()
            .find(|repo| repo.name == "extra")
            .expect("extra repo present in workspace");
        let checkout = git2::Repository::open(&extra_repo.worktree_path).unwrap();
        assert_eq!(
            checkout.head().unwrap().peel_to_commit().unwrap().id(),
            extra_release_tip,
            "extra repo worktree should branch from its configured `release` base"
        );
        assert_eq!(extra_repo.base_branch.as_deref(), Some("release"));
        assert_eq!(
            workspace
                .repos
                .iter()
                .find(|repo| repo.name == "primary")
                .unwrap()
                .base_branch,
            None,
            "a repo with no configured base records none, so the diff falls through to detection"
        );
        assert!(result
            .created_workspace_worktrees
            .iter()
            .all(|worktree| worktree.owned_branch.as_deref() == Some("feature-x")));
        let created = result
            .created_workspace_worktrees
            .iter()
            .map(|worktree| (worktree.path.clone(), worktree.main_repo_path.clone()))
            .collect::<Vec<_>>();
        drop(checkout);

        result.rollback().unwrap();
        for (path, repository) in created {
            assert!(!path.exists());
            assert!(git2::Repository::open(repository)
                .unwrap()
                .find_branch("feature-x", git2::BranchType::Local)
                .is_err());
        }
    }

    #[test]
    fn create_workspace_records_no_base_when_attaching_an_existing_branch() {
        let _home = crate::session::test_support::isolate_app_dir();
        let (parent_primary, _) = init_repo_with_branch("primary", "feature-x");
        let primary = parent_primary.path().join("primary");
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();

        let mut result = provision_workspace_for_test(
            &WorkspaceRepoSpec {
                path: primary,
                base_branch: Some("main".to_string()),
            },
            &[],
            "feature-x",
            false,
            &template,
            true,
        )
        .expect("workspace creation should succeed");

        assert_eq!(
            result.instance.workspace_info.as_ref().unwrap().repos[0].base_branch,
            None
        );
        assert_eq!(result.created_workspace_worktrees[0].owned_branch, None);
        let path = result.created_workspace_worktrees[0].path.clone();
        let repository = result.created_workspace_worktrees[0].main_repo_path.clone();

        result.rollback().unwrap();
        assert!(!path.exists());
        assert!(git2::Repository::open(repository)
            .unwrap()
            .find_branch("feature-x", git2::BranchType::Local)
            .is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn creation_rollback_keeps_owned_branch_when_worktree_removal_fails() {
        let _home = super::super::test_support::isolate_app_dir();
        let (parent, _) = init_repo_with_branch("cleanup", "release");
        let main_repo_path = parent.path().join("cleanup");
        let mut result = provision_workspace_for_test(
            &WorkspaceRepoSpec {
                path: main_repo_path.clone(),
                base_branch: Some("release".to_owned()),
            },
            &[],
            "rollback-branch",
            true,
            parent.path().join("workspace/{branch}").to_str().unwrap(),
            false,
        )
        .unwrap();
        let worktree_path = result.created_workspace_worktrees[0].path.clone();
        std::fs::write(worktree_path.join("README.md"), "dirty\n").unwrap();

        assert!(result.rollback().is_err());
        assert_eq!(
            std::fs::read(worktree_path.join("README.md")).unwrap(),
            b"dirty\n"
        );
        let repo = git2::Repository::open(&main_repo_path).unwrap();
        assert!(
            repo.find_branch("rollback-branch", git2::BranchType::Local)
                .is_ok(),
            "owned branch must not be deleted while its worktree remains"
        );

        let git = GitWorktree::new(main_repo_path).unwrap();
        git.remove_worktree(&worktree_path, true).unwrap();
        git.delete_branch("rollback-branch").unwrap();
    }

    #[test]
    fn resolve_repo_base_selectors_matches_name_or_path() {
        let repos = vec![
            PathBuf::from("/src/app"),
            PathBuf::from("/src/api"),
            PathBuf::from("/elsewhere/web"),
        ];

        let out = resolve_repo_base_selectors(
            &repos,
            &[
                ("api".to_string(), "epic/checkout".to_string()),
                ("/elsewhere/web".to_string(), " develop ".to_string()),
            ],
        )
        .expect("both selectors resolve");
        assert_eq!(
            out.get(&PathBuf::from("/src/api")).map(String::as_str),
            Some("epic/checkout")
        );
        assert_eq!(
            out.get(&PathBuf::from("/elsewhere/web"))
                .map(String::as_str),
            Some("develop")
        );
        assert!(!out.contains_key(&PathBuf::from("/src/app")));

        assert!(resolve_repo_base_selectors(&repos, &[]).unwrap().is_empty());

        let cases = [
            (
                vec![("nope".to_string(), "develop".to_string())],
                "No repo named",
            ),
            (
                vec![("api".to_string(), "  ".to_string())],
                "No base branch",
            ),
            (
                vec![
                    ("api".to_string(), "develop".to_string()),
                    ("/src/api".to_string(), "main".to_string()),
                ],
                "twice",
            ),
        ];
        for (pairs, expected) in cases {
            let err = resolve_repo_base_selectors(&repos, &pairs)
                .expect_err("should reject")
                .to_string();
            assert!(err.contains(expected), "got: {err}");
        }

        let subdir = vec![PathBuf::from("/src/api/crates/core")];
        assert!(
            resolve_repo_base_selectors(&subdir, &[("api".to_string(), "develop".to_string())])
                .is_err(),
            "a repo name must not resolve against a subdirectory path"
        );
        assert!(resolve_repo_base_selectors(
            &subdir,
            &[("core".to_string(), "develop".to_string())]
        )
        .is_ok());

        let dupes = vec![PathBuf::from("/a/api"), PathBuf::from("/b/api")];
        let err = resolve_repo_base_selectors(&dupes, &[("api".to_string(), "x".to_string())])
            .expect_err("ambiguous name")
            .to_string();
        assert!(err.contains("ambiguous"), "got: {err}");
    }

    fn isolated_app_dir(temp_home: &std::path::Path) -> std::path::PathBuf {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let config_home = temp_home.join(".config");

            config_home.join(crate::session::APP_DIR_NAME_XDG)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            temp_home.join(crate::session::APP_DIR_NAME_OTHER)
        }
    }

    fn custom_agent_params(project_path: &std::path::Path, tool: &str) -> InstanceParams {
        InstanceParams {
            provisional_creation: None,
            has_creation_callbacks: false,
            title: "custom session".to_string(),
            title_typed: false,
            path: project_path.to_string_lossy().to_string(),
            group: String::new(),
            tool: tool.to_string(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: "ubuntu:latest".to_string(),
            yolo_mode: false,
            extra_env: Vec::new(),
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: Vec::new(),
            repo_base_branches: Vec::new(),
            scratch: false,
            fork_seed: None,
        }
    }

    #[test]
    fn apply_agent_launch_config_prefers_set_session_values_over_config() {
        // (session extra, config extra, session command, config override,
        //  session yolo, config yolo) -> (extra, command, yolo)
        let cases = [
            (("", None, "", None, None, false), ("", "claude", false)),
            (
                ("", Some("--cfg"), "", Some("wrap"), None, true),
                ("--cfg", "wrap", true),
            ),
            (
                ("", Some(""), "", Some(""), None, false),
                ("", "claude", false),
            ),
            (
                (
                    "--mine",
                    Some("--cfg"),
                    "mine",
                    Some("wrap"),
                    Some(false),
                    true,
                ),
                ("--mine", "mine", false),
            ),
        ];
        for ((extra, cfg_extra, cmd, cfg_cmd, yolo, cfg_yolo), expected) in cases {
            let mut session = crate::session::config::SessionConfig {
                yolo_mode_default: cfg_yolo,
                ..Default::default()
            };
            if let Some(v) = cfg_extra {
                session.agent_extra_args.insert("claude".into(), v.into());
            }
            if let Some(v) = cfg_cmd {
                session
                    .agent_command_override
                    .insert("claude".into(), v.into());
            }
            let mut inst = Instance::new("t", "/p");
            inst.tool = "claude".into();
            inst.command = "claude".into();
            apply_agent_launch_config(&mut inst, &session, extra, cmd, yolo);
            assert_eq!(
                (
                    inst.extra_args.as_str(),
                    inst.command.as_str(),
                    inst.yolo_mode
                ),
                expected,
                "extra={extra:?} cfg_extra={cfg_extra:?} cmd={cmd:?} cfg_cmd={cfg_cmd:?}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn only_a_typed_title_names_the_agent() {
        let temp_home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let project = tempfile::tempdir().unwrap();
        for (typed, expected) in [(true, true), (false, false)] {
            let mut params = custom_agent_params(project.path(), "claude");
            params.title_typed = typed;
            let instance = build_instance(params, &[], &[], "default")
                .unwrap()
                .instance;
            assert_eq!(instance.first_launch_names_agent, expected, "typed {typed}");
        }
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_resolves_custom_agent_commands_and_detect_as() {
        let temp_home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                remote-claude = "ssh -t host claude"
                remote-opencode = "ssh -t host opencode"

                whitespace-agent = "   "

                [session.agent_detect_as]
                remote-claude = "claude"
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("default");

        let result = build_instance(
            custom_agent_params(project.path(), "remote-claude"),
            &[],
            &[],
            "default",
        )
        .unwrap();

        assert_eq!(result.instance.tool, "remote-claude");
        assert_eq!(result.instance.command, "ssh -t host claude");
        assert_eq!(result.instance.detect_as, "claude");

        let unmapped = build_instance(
            custom_agent_params(project.path(), "remote-opencode"),
            &[],
            &[],
            "default",
        )
        .unwrap();
        assert_eq!(unmapped.instance.command, "ssh -t host opencode");
        assert_eq!(unmapped.instance.detect_as, "");

        for tool in ["remote-missing", "whitespace-agent"] {
            let Err(err) = build_instance(
                custom_agent_params(project.path(), tool),
                &[],
                &[],
                "default",
            ) else {
                panic!("{tool}: custom agent without a command should fail");
            };
            assert!(
                err.to_string().contains(&format!(
                    "No launch command resolved for custom agent '{tool}'"
                )),
                "unexpected error: {err}"
            );
        }
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_provisions_scratch_and_rejects_invalid_worktree_requests() {
        let temp_home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), "").unwrap();

        let mut params = custom_agent_params(std::path::Path::new(""), "claude");
        params.scratch = true;
        let result = build_instance(params.clone(), &[], &[], "default")
            .expect("scratch build must succeed without a project path");
        assert!(
            result.instance.scratch,
            "scratch flag must be persisted on the instance"
        );
        let provisioned = std::path::PathBuf::from(&result.instance.project_path);
        assert!(provisioned.exists());
        assert!(super::super::scratch::is_scratch_path(&provisioned));
        let _ = std::fs::remove_dir_all(&provisioned);

        params.worktree_enabled = true;
        params.worktree_branch = Some("feat".to_string());
        let Err(err) = build_instance(params, &[], &[], "default") else {
            panic!("scratch + worktree must error");
        };
        assert!(
            err.to_string()
                .contains("Cannot combine --scratch with worktree mode"),
            "unexpected error: {err}"
        );

        let project = tempfile::tempdir().unwrap();
        let mut params = custom_agent_params(project.path(), "claude");
        params.worktree_enabled = true;
        params.worktree_branch = Some("feat".to_string());
        let Err(err) = build_instance(params, &[], &[], "default") else {
            panic!("worktree on a non-git path must error");
        };
        assert!(
            err.chain()
                .filter_map(|c| c.downcast_ref::<crate::git::error::GitError>())
                .any(|g| matches!(g, crate::git::error::GitError::NotAGitRepo)),
            "expected a typed GitError::NotAGitRepo in the chain, got: {err:#}"
        );
    }

    fn build_instance_applies_structured_fork_seed() {
        use crate::session::ForkSeed;
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("default");
        let params = InstanceParams {
            provisional_creation: None,
            has_creation_callbacks: false,
            title: "Forked".into(),
            title_typed: false,
            path: "/tmp".into(),
            group: String::new(),
            tool: "claude".into(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: String::new(),
            yolo_mode: false,
            extra_env: vec![],
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: vec![],
            repo_base_branches: Vec::new(),
            scratch: false,
            fork_seed: Some(ForkSeed::Structured {
                parent_acp_session_id: "parent-acp-id".into(),
            }),
        };
        let inst = build_instance(params, &[], &[], "default")
            .unwrap()
            .instance;
        assert_eq!(inst.view, crate::session::View::Structured);
        assert_eq!(inst.fork_pending.as_deref(), Some("parent-acp-id"));
        assert_eq!(inst.import_pending, Some(true));
        assert!(inst.agent_session_id.is_none());
        assert!(!matches!(
            inst.resume_intent,
            crate::session::instance::ResumeIntent::Fork { .. }
        ));
    }

    fn build_instance_applies_terminal_fork_seed() {
        use crate::session::ForkSeed;
        let _registry = crate::tmux::status_rules::ProfileRegistryGuard::take("default");
        // The CLI e2e covers the separate application in `add.rs`; this is the
        // arm `build_instance` owns, which pins the child conversation and the
        // parent the first launch must fork from.
        let parent = crate::session::ConversationBinding {
            session_id: "parent-conversation".into(),
            execution: Some(crate::session::ExecutionBinding {
                agent: "claude".into(),
                stores: vec![std::path::PathBuf::from("/tmp/store")],
                configuration: Vec::new(),
                cwd: "/tmp".into(),
                cwd_filesystem: "host".into(),
                filesystem: "host".into(),
                exported_default_store: None,
            }),
            provenance: crate::session::ConversationProvenance::Observed,
            transcript_path: None,
        };
        let params = InstanceParams {
            provisional_creation: None,
            has_creation_callbacks: false,
            title: "Forked".into(),
            title_typed: false,
            path: "/tmp".into(),
            group: String::new(),
            tool: "claude".into(),
            worktree_enabled: false,
            worktree_branch: None,
            create_new_branch: false,
            base_branch: None,
            sandbox: false,
            sandbox_image: String::new(),
            yolo_mode: false,
            extra_env: vec![],
            extra_args: String::new(),
            command_override: String::new(),
            extra_repo_paths: vec![],
            repo_base_branches: Vec::new(),
            scratch: false,
            fork_seed: Some(ForkSeed::Terminal {
                parent: Box::new(parent.clone()),
                child_session_id: "child-conversation".into(),
                unattributed_parent_agent: None,
            }),
        };
        let inst = build_instance(params, &[], &[], "default")
            .unwrap()
            .instance;
        assert_eq!(inst.agent_session_id.as_deref(), Some("child-conversation"));
        assert_eq!(
            inst.resume_intent,
            crate::session::ResumeIntent::Fork {
                from: "parent-conversation".into()
            }
        );
        assert_eq!(inst.resume_binding.as_ref(), Some(&parent));
    }

    #[test]
    #[serial_test::serial]
    fn fork_seed_builds_apply_the_seed_and_restore_default_profile_registry() {
        let _app_guard = crate::session::test_support::isolate_app_dir();
        const ALIAS_AGENT: &str = "fork-seed-registry-alias";
        const RULE_AGENT: &str = "fork-seed-registry-rule";
        let cases: &[(&str, fn())] = &[
            ("terminal", build_instance_applies_terminal_fork_seed),
            ("structured", build_instance_applies_structured_fork_seed),
        ];

        for (label, run) in cases {
            let _cleanup = crate::tmux::status_rules::ProfileRegistryGuard::take("default");
            let mut sentinels = crate::session::Config::default();
            sentinels
                .session
                .agent_detect_as
                .insert(ALIAS_AGENT.to_string(), "codex".to_string());
            sentinels
                .agents
                .entry(RULE_AGENT.to_string())
                .or_default()
                .status_rules = vec![crate::session::config::StatusRule {
                status: crate::agents::HookStatus::Running,
                contains: Some("fork-seed-working".to_string()),
                regex: None,
            }];
            crate::tmux::status_rules::install_from_config("default", &sentinels);

            run();

            let alias = crate::tmux::status_rules::effective_detect_as("default", ALIAS_AGENT, "");
            let rule =
                crate::tmux::status_rules::detect("default", RULE_AGENT, "fork-seed-working");
            assert_eq!(
                (alias.as_ref(), rule),
                ("codex", Some(crate::session::Status::Running)),
                "{label}: fork-seed build must restore the prior alias and compiled rule"
            );
        }
    }

    /// The parent's capability is checked against the parent row's own agent,
    /// and the launch skips identity checking for an unattributed binding, so
    /// a child that would launch another agent must be refused here rather
    /// than fork a conversation it cannot resume.
    #[test]
    #[serial_test::serial]
    fn a_fork_child_that_would_launch_another_agent_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        let mut parent = crate::session::Instance::new("parent", root.path().to_str().unwrap());
        parent.tool = "claude".into();
        parent.agent_session_id = Some("legacy-uuid".into());
        parent.agent_session_binding =
            Some(crate::session::ConversationBinding::unknown("legacy-uuid"));
        let seed = crate::session::fork::terminal_fork_seed(
            parent.fork_parent_ref().unwrap(),
            "child-uuid".into(),
            false,
        )
        .expect("an unattributed parent is admitted");

        let mut same_agent = custom_agent_params(root.path(), "claude");
        same_agent.command_override = "claude".into();
        same_agent.fork_seed = Some(seed.clone());
        assert_eq!(
            build_instance(same_agent, &[], &[], "default")
                .expect("a child launching the parent's own agent still forks")
                .instance
                .agent_session_id
                .as_deref(),
            Some("child-uuid")
        );

        let mut other_agent = custom_agent_params(root.path(), "codex");
        other_agent.command_override = "codex".into();
        other_agent.fork_seed = Some(seed);
        let refused = match build_instance(other_agent, &[], &[], "default") {
            Ok(_) => panic!("a child launching another agent cannot carry the conversation"),
            Err(error) => error.to_string(),
        };
        assert!(
            refused.contains("codex") && refused.contains("claude"),
            "the refusal must name both agents: {refused}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn detached_creation_supports_multiple_remote_refs_without_local_branches() {
        let _home = super::super::test_support::isolate_app_dir();
        let (parent, tip) = init_repo_with_branch("detached-specs", "release");
        let main = parent.path().join("detached-specs");
        let repo = git2::Repository::open(&main).unwrap();
        repo.reference("refs/remotes/origin/main", tip, true, "fixture")
            .unwrap();
        let base_tip = repo.find_commit(tip).unwrap().parent_id(0).unwrap();
        repo.reference("refs/remotes/upstream/topic", base_tip, true, "fixture")
            .unwrap();

        for requested in ["origin/main", "upstream/topic"] {
            let expected = repo
                .revparse_single(requested)
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id();
            let mut params = custom_agent_params(&main, "claude");
            params.worktree_enabled = true;
            params.worktree_branch = Some(requested.into());
            params.create_new_branch = false;
            let mut result = build_instance(params, &[], &[], "default").unwrap();
            let path = result.created_worktree.as_ref().unwrap().path.clone();

            assert_eq!(
                result.instance.worktree_info.as_ref().unwrap().branch,
                requested
            );
            let checkout = git2::Repository::open(&path).unwrap();
            assert!(checkout.head_detached().unwrap(), "{requested}");
            assert_eq!(
                checkout.head().unwrap().peel_to_commit().unwrap().id(),
                expected
            );
            drop(checkout);
            result.rollback().unwrap();
            assert!(!path.exists(), "{requested}: checkout was not rolled back");
        }

        assert_eq!(
            repo.find_reference("refs/remotes/origin/main")
                .unwrap()
                .target(),
            Some(tip)
        );
        assert!(repo.find_reference("refs/heads/origin/main").is_err());
        assert_eq!(
            repo.find_reference("refs/remotes/upstream/topic")
                .unwrap()
                .target(),
            Some(base_tip)
        );
        assert!(repo.find_reference("refs/heads/upstream/topic").is_err());
    }

    #[test]
    #[serial_test::serial]
    fn remote_suffix_and_explicit_remote_publish_their_actual_checkout_kinds() {
        let _home = super::super::test_support::isolate_app_dir();
        let (parent, tip) = init_repo_with_branch("remote-mode", "release");
        let main = parent.path().join("remote-mode");
        let repo = git2::Repository::open(&main).unwrap();
        repo.remote("origin", main.to_str().unwrap()).unwrap();
        repo.reference("refs/remotes/origin/remote-topic", tip, true, "fixture")
            .unwrap();

        for (requested, detached) in [("origin/remote-topic", true), ("remote-topic", false)] {
            let mut params = custom_agent_params(&main, "claude");
            params.worktree_enabled = true;
            params.worktree_branch = Some(requested.into());
            params.create_new_branch = false;
            let mut result = build_instance(params, &[], &[], "default").unwrap();
            let path = result.created_worktree.as_ref().unwrap().path.clone();

            let checkout = git2::Repository::open(&path).unwrap();
            assert_eq!(checkout.head_detached().unwrap(), detached, "{requested}");
            assert_eq!(checkout.head().unwrap().peel_to_commit().unwrap().id(), tip);
            if !detached {
                assert_eq!(
                    checkout.head().unwrap().name().unwrap(),
                    "refs/heads/remote-topic"
                );
            }
            drop(checkout);
            result.rollback().unwrap();
            assert!(!path.exists(), "{requested}: checkout was not rolled back");
        }

        assert_eq!(
            repo.find_reference("refs/remotes/origin/remote-topic")
                .unwrap()
                .target(),
            Some(tip)
        );
        assert_eq!(
            repo.find_reference("refs/heads/remote-topic")
                .unwrap()
                .target(),
            Some(tip)
        );
        assert!(repo
            .find_reference("refs/heads/origin/remote-topic")
            .is_err());
    }

    #[test]
    #[serial_test::serial]
    fn detached_hook_commit_is_published() {
        use std::os::unix::fs::PermissionsExt;
        let _home = super::super::test_support::isolate_app_dir();
        let (parent, tip) = init_repo_with_branch("detached-commit", "release");
        let main = parent.path().join("detached-commit");
        let repo = git2::Repository::open(&main).unwrap();
        repo.reference("refs/remotes/origin/main", tip, true, "fixture")
            .unwrap();
        let marker = parent.path().join("hook-completed");
        let hook = repo.path().join("hooks/post-checkout");
        std::fs::write(&hook, format!(
            "#!/bin/sh\nunset GIT_DIR GIT_WORK_TREE GIT_PREFIX GIT_INDEX_FILE\ngit -C \"$PWD\" -c user.name=fixture -c user.email=fixture@example.com commit --allow-empty -m detached-hook >/dev/null || exit 91\nprintf complete > {}\n",
            super::super::environment::shell_escape(&marker.to_string_lossy()),
        )).unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut params = custom_agent_params(&main, "claude");
        params.worktree_enabled = true;
        params.worktree_branch = Some("origin/main".into());
        params.create_new_branch = false;
        let mut result = build_instance(params, &[], &[], "default").unwrap();
        let path = result.created_worktree.as_ref().unwrap().path.clone();
        let checkout = git2::Repository::open(&path).unwrap();
        let committed = checkout.head().unwrap().peel_to_commit().unwrap().id();
        assert!(checkout.head_detached().unwrap());
        assert_ne!(committed, tip);
        assert_eq!(std::fs::read(&marker).unwrap(), b"complete");
        assert_eq!(
            repo.find_reference("refs/remotes/origin/main")
                .unwrap()
                .target(),
            Some(tip)
        );
        drop(checkout);
        result.run_on_create(|_| Ok(())).unwrap();
        result.publish().unwrap();
        assert_eq!(
            git2::Repository::open(&path)
                .unwrap()
                .head()
                .unwrap()
                .peel_to_commit()
                .unwrap()
                .id(),
            committed,
        );
        assert!(result
            .profile_storage
            .load()
            .unwrap()
            .iter()
            .any(|row| row.id == result.instance.id && row.project_path == path.to_string_lossy()));
    }
}
