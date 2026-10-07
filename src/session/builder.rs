//! Instance creation and canonical filesystem publication.

use std::{collections::HashSet, path::PathBuf};

use anyhow::{bail, Result};
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
}

/// A prepared instance and its original publication custody.
pub struct BuildResult {
    pub instance: Instance,
    /// Non-fatal warnings from worktree/workspace creation. Callers should
    /// surface these to the user (post-checkout hook failures etc.).
    pub warnings: Vec<String>,
    /// Original filesystem custody for builds that reserve paths before effects.
    pub creation_intent: std::sync::Arc<CreationIntent>,
}

/// Result of creating a multi-repo workspace.
pub struct WorkspaceResult {
    pub workspace_info: WorkspaceInfo,
    pub workspace_path: PathBuf,
    /// Non-fatal warnings from worktree creation (e.g. post-checkout hook
    /// failures where the worktree itself was created successfully).
    pub warnings: Vec<String>,
    pub(crate) creation_intent: std::sync::Arc<CreationIntent>,
}

/// Filesystem-only custody issued by a successful canonical write, never native authority.
pub struct CreationIntent {
    storage: std::sync::Arc<super::Storage>,
    acknowledged: Instance,
    ready_status: super::Status,
    paths: Vec<PathBuf>,
}

impl std::fmt::Debug for CreationIntent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CreationIntent")
            .field("session_id", &self.acknowledged.id)
            .finish_non_exhaustive()
    }
}

impl CreationIntent {
    /// Publish the prepared build through its original physical profile.
    pub fn publish(&self, prepared: &Instance) -> Result<Instance> {
        let _workspace = super::acquire_session_workspace_claim_lock()?;
        let _identity = super::acquire_session_identity_lock()?;
        publish_prepared_creation_under_workspace_claim_lock(
            self.storage(),
            prepared,
            self,
            |rows, groups| {
                if !prepared.group_path.is_empty() {
                    let mut tree = super::GroupTree::new_with_groups(rows, groups);
                    tree.create_group(&prepared.group_path);
                    *groups = tree.get_all_groups();
                }
                Ok(())
            },
        )
    }
    pub(crate) fn reserve(
        storage: &super::Storage,
        prepared: &mut Instance,
    ) -> Result<std::sync::Arc<Self>> {
        let paths = prepared
            .durable_worktree_paths()
            .map(PathBuf::from)
            .collect();
        Self::reserve_paths(storage, prepared, paths)
    }

    pub(crate) fn reserve_metadata(
        storage: &super::Storage,
        prepared: &mut Instance,
    ) -> Result<std::sync::Arc<Self>> {
        Self::reserve_paths(storage, prepared, Vec::new())
    }

    fn reserve_paths(
        storage: &super::Storage,
        prepared: &mut Instance,
        paths: Vec<PathBuf>,
    ) -> Result<std::sync::Arc<Self>> {
        if let Some(origin) = &prepared.storage_origin {
            anyhow::ensure!(
                storage.same_origin_as(origin),
                "creation changed its physical profile"
            );
        } else {
            prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));
        }
        prepared.source_profile = storage.profile().to_owned();
        let mut reserved = prepared.clone();
        let ready_status = prepared.status;
        reserved.status = super::Status::Creating;
        reserved.try_acquire_lifecycle_reservation(
            super::LifecycleOperation::Create,
            Instance::LIFECYCLE_RESERVATION_TTL,
            Utc::now(),
        )?;
        reserved.lifecycle_reservation.as_mut().unwrap().path_claims =
            super::WorktreePathClaims::Pending(paths.clone());
        let acknowledged = storage.update(|rows, _groups| {
            if super::is_duplicate_session(
                rows.iter(),
                &reserved.title,
                &reserved.project_path,
                None,
            ) {
                return Err(super::duplicate_session_error(&reserved.title));
            }
            anyhow::ensure!(
                !rows.iter().any(|row| row.id == reserved.id),
                "creation identity is already owned"
            );
            if !paths.is_empty() {
                let mut claims = super::deletion::PathClaimIndex::load_for_writer(
                    std::slice::from_ref(storage),
                )?;
                let profile = claims
                    .take_targets()
                    .into_iter()
                    .next()
                    .ok_or_else(|| {
                        anyhow::anyhow!("creation profile is absent from claim inventory")
                    })?
                    .1;
                claims.ensure_unclaimed(profile, &reserved.id, &paths)?;
            }
            rows.push(reserved.clone());
            Ok(reserved)
        })?;
        prepared.lifecycle_generation = acknowledged.lifecycle_generation;
        prepared.lifecycle_reservation = acknowledged.lifecycle_reservation.clone();
        Ok(std::sync::Arc::new(Self {
            storage: std::sync::Arc::new(storage.clone()),
            acknowledged,
            ready_status,
            paths,
        }))
    }

    pub(crate) fn storage(&self) -> &super::Storage {
        &self.storage
    }
    pub(crate) fn session_id(&self) -> &str {
        &self.acknowledged.id
    }

    pub(crate) fn validate_row(&self, row: &Instance) -> Result<()> {
        self.storage.verify_profile_identity()?;
        anyhow::ensure!(
            row.id == self.acknowledged.id
                && row.created_at == self.acknowledged.created_at
                && row.lifecycle_reservation == self.acknowledged.lifecycle_reservation
                && self.same_filesystem_plan(row),
            "creation filesystem custody was superseded"
        );
        anyhow::ensure!(row.lifecycle_reservation.as_ref().is_some_and(|lease| {
            lease.op == super::LifecycleOperation::Create
                && matches!(&lease.path_claims, super::WorktreePathClaims::Pending(paths) if paths == &self.paths)
        }), "creation has no acknowledged complete filesystem plan");
        Ok(())
    }

    fn same_filesystem_plan(&self, row: &Instance) -> bool {
        let original = &self.acknowledged;
        fn worktree(row: &Instance) -> Option<(&str, &str, bool, Option<&str>)> {
            row.worktree_info.as_ref().map(|info| {
                (
                    info.branch.as_str(),
                    info.main_repo_path.as_str(),
                    info.managed_by_aoe,
                    info.base_branch.as_deref(),
                )
            })
        }
        if row.project_path != original.project_path
            || row.scratch != original.scratch
            || worktree(row) != worktree(original)
        {
            return false;
        }
        match (&row.workspace_info, &original.workspace_info) {
            (None, None) => true,
            (Some(current), Some(original)) => {
                current.workspace_dir == original.workspace_dir
                    && current.branch == original.branch
                    && current.cleanup_on_delete == original.cleanup_on_delete
                    && current
                        .repos
                        .iter()
                        .map(|repo| {
                            (
                                &repo.name,
                                &repo.source_path,
                                &repo.branch,
                                &repo.worktree_path,
                                &repo.main_repo_path,
                                repo.managed_by_aoe,
                                repo.branch_preexisting,
                                &repo.base_branch,
                            )
                        })
                        .eq(original.repos.iter().map(|repo| {
                            (
                                &repo.name,
                                &repo.source_path,
                                &repo.branch,
                                &repo.worktree_path,
                                &repo.main_repo_path,
                                repo.managed_by_aoe,
                                repo.branch_preexisting,
                                &repo.base_branch,
                            )
                        }))
            }
            _ => false,
        }
    }

    fn prepared_row(&self, canonical: &Instance, prepared: &Instance) -> Result<Instance> {
        self.validate_row(canonical)?;
        anyhow::ensure!(
            prepared.id == canonical.id
                && prepared.created_at == canonical.created_at
                && self.same_filesystem_plan(prepared),
            "prepared creation changed its immutable filesystem plan"
        );
        anyhow::ensure!(
            prepared
                .storage_origin
                .as_ref()
                .is_some_and(|origin| self.storage.same_origin_as(origin))
                && prepared.source_profile == self.storage.profile(),
            "prepared creation changed its original physical profile"
        );
        let mut row = prepared.clone();
        row.runner_journal = canonical.runner_journal.clone();
        row.active_execution = canonical.active_execution.clone();
        row.lifecycle_generation = canonical.lifecycle_generation;
        row.lifecycle_reservation = canonical.lifecycle_reservation.clone();
        Ok(row)
    }

    pub(crate) fn refresh_prepared(&self, prepared: &Instance) -> Result<()> {
        self.storage.update_metadata(|rows, _groups| {
            let row = rows
                .iter_mut()
                .find(|row| row.id == self.session_id())
                .ok_or_else(|| anyhow::anyhow!("creation filesystem owner disappeared"))?;
            let mut refreshed = self.prepared_row(row, prepared)?;
            refreshed.status = super::Status::Creating;
            *row = refreshed;
            Ok(())
        })
    }

    pub(crate) fn publish_under_workspace_claim_lock<F, R>(
        &self,
        prepared: &Instance,
        publish: F,
    ) -> Result<R>
    where
        F: FnOnce(&mut Vec<Instance>, &mut Vec<super::Group>, Instance) -> Result<R>,
    {
        self.storage
            .complete_creation_under_workspace_claim_lock(self, |rows, groups| {
                let canonical = rows
                    .iter()
                    .find(|row| row.id == self.session_id())
                    .ok_or_else(|| anyhow::anyhow!("creation filesystem owner disappeared"))?;
                let mut committed = self.prepared_row(canonical, prepared)?;
                committed.lifecycle_reservation = None;
                if committed.status == super::Status::Creating {
                    committed.status = self.ready_status;
                }
                publish(rows, groups, committed)
            })
    }
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

/// Create a multi-repo workspace with worktrees for each repository.
pub(crate) fn create_workspace(
    primary: &WorkspaceRepoSpec,
    extra_repos: &[WorkspaceRepoSpec],
    branch: &str,
    create_new_branch: bool,
    workspace_template: &str,
    init_submodules: bool,
    prepared: &mut Instance,
) -> Result<WorkspaceResult> {
    let storage = prepared.original_storage()?;
    let primary_main_repo = GitWorktree::find_main_repo(&primary.path)?;
    let primary_git_wt = GitWorktree::new(primary_main_repo)?;

    let session_id_short = &prepared.id[..8];

    let workspace_path =
        primary_git_wt.compute_path(branch, workspace_template, session_id_short)?;
    anyhow::ensure!(
        !workspace_path.try_exists()?,
        "workspace destination already exists"
    );
    let workspace_dir = workspace_path.to_string_lossy().to_string();

    // (canonicalized path, resolved base branch) for the primary repo followed by every extra repo.
    let all_repos: Vec<(PathBuf, Option<String>)> =
        std::iter::once((primary.path.clone(), primary.base_branch.clone()))
            .chain(extra_repos.iter().map(|r| {
                (
                    r.path.canonicalize().unwrap_or_else(|_| r.path.clone()),
                    r.base_branch.clone(),
                )
            }))
            .collect();

    // Check for duplicate repo directory names
    let mut seen_names = std::collections::HashSet::new();
    for (repo_path, _) in &all_repos {
        let name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());
        if !seen_names.insert(name.clone()) {
            bail!(
                "Duplicate repository name '{}' in workspace\n\
                 Tip: Rename one of the directories to avoid the collision",
                name
            );
        }
    }

    // Resolve every repository before reserving or mutating a path.
    struct RepoPlan {
        repo_path: PathBuf,
        repo_name: String,
        main_repo_path: PathBuf,
        worktree_subdir: PathBuf,
        base_branch: Option<String>,
    }
    let mut plans: Vec<RepoPlan> = Vec::with_capacity(all_repos.len());
    for (repo_path, base_branch) in &all_repos {
        if !GitWorktree::is_git_repo(repo_path) {
            bail!(
                "Path is not in a git repository: {}\n\
                 Tip: All --repo paths must be git repositories",
                repo_path.display()
            );
        }

        let main_repo_path_raw = GitWorktree::find_main_repo(repo_path)?;
        let main_repo_path = main_repo_path_raw
            .canonicalize()
            .unwrap_or(main_repo_path_raw);

        let repo_name = repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "repo".to_string());

        let worktree_subdir = workspace_path.join(&repo_name);

        plans.push(RepoPlan {
            repo_path: repo_path.clone(),
            repo_name,
            main_repo_path,
            worktree_subdir,
            base_branch: base_branch.clone(),
        });
    }
    let workspace_info = WorkspaceInfo {
        branch: branch.to_string(),
        workspace_dir,
        created_at: Utc::now(),
        cleanup_on_delete: true,
        repos: plans
            .iter()
            .map(|plan| WorkspaceRepo {
                name: plan.repo_name.clone(),
                source_path: plan.repo_path.to_string_lossy().into_owned(),
                branch: branch.to_string(),
                worktree_path: plan.worktree_subdir.to_string_lossy().into_owned(),
                main_repo_path: plan.main_repo_path.to_string_lossy().into_owned(),
                managed_by_aoe: true,
                branch_preexisting: false,
                base_branch: create_new_branch
                    .then(|| plan.base_branch.clone())
                    .flatten(),
                base_branch_override: None,
            })
            .collect(),
    };
    prepared.project_path = workspace_path.to_string_lossy().into_owned();
    prepared.workspace_info = Some(workspace_info.clone());
    let creation_intent = CreationIntent::reserve(&storage, prepared)?;
    std::fs::create_dir_all(&workspace_path)?;

    // Run create_worktree for every repo concurrently.
    let create_start = std::time::Instant::now();
    let parallel_results: Vec<std::result::Result<Vec<String>, String>> =
        std::thread::scope(|scope| {
            let handles: Vec<_> = plans
                .iter()
                .map(|plan| {
                    let branch = branch.to_string();
                    let base = plan.base_branch.clone();
                    let main_repo_path = plan.main_repo_path.clone();
                    let worktree_subdir = plan.worktree_subdir.clone();
                    let repo_name = plan.repo_name.clone();
                    scope.spawn(move || -> std::result::Result<Vec<String>, String> {
                        let repo_start = std::time::Instant::now();
                        let result = (|| -> std::result::Result<Vec<String>, String> {
                            let git_wt = GitWorktree::new(main_repo_path)
                                .map_err(|e| format!("{}: {}", repo_name, e))?
                                .with_init_submodules(init_submodules);
                            git_wt
                                .create_worktree(
                                    &branch,
                                    &worktree_subdir,
                                    create_new_branch,
                                    base.as_deref(),
                                )
                                .map_err(|e| format!("{}: {}", repo_name, e))
                        })();
                        tracing::info!(target: "session.create",
                            "workspace create: repo={} elapsed={:?} ok={}",
                            repo_name,
                            repo_start.elapsed(),
                            result.is_ok()
                        );
                        result
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| match h.join() {
                    Ok(r) => r,
                    Err(_) => Err("worktree thread panicked".to_string()),
                })
                .collect()
        });
    tracing::info!(target: "session.create",
        "workspace create: {} repos completed in {:?}",
        plans.len(),
        create_start.elapsed()
    );

    let mut warnings: Vec<String> = Vec::new();
    let mut errors: Vec<String> = Vec::new();

    for result in parallel_results {
        match result {
            Ok(w) => {
                warnings.extend(w);
            }
            Err(msg) => errors.push(msg),
        }
    }

    if !errors.is_empty() {
        tracing::warn!(target: "session.create", session = %prepared.id, "Retaining durable creation intent after an uncertain Git outcome");
        if errors.len() == 1 {
            bail!("Failed to create worktree for {}", errors.remove(0));
        } else {
            bail!(
                "Failed to create worktrees ({} repos):\n  - {}",
                errors.len(),
                errors.join("\n  - ")
            );
        }
    }

    Ok(WorkspaceResult {
        workspace_info,
        workspace_path,
        warnings,
        creation_intent,
    })
}

/// Build an instance with all setup (worktree resolution, sandbox config).
pub fn build_instance(
    params: InstanceParams,
    existing_titles: &[&str],
    existing_branches: &[&str],
    storage: &super::Storage,
) -> Result<BuildResult> {
    build_instance_from_admitted(
        params,
        existing_titles,
        existing_branches,
        Instance::new("", ""),
        storage,
    )
}

pub(crate) fn build_instance_from_admitted(
    params: InstanceParams,
    existing_titles: &[&str],
    existing_branches: &[&str],
    mut instance: Instance,
    storage: &super::Storage,
) -> Result<BuildResult> {
    storage.verify_profile_identity()?;
    let profile = storage.profile();
    instance.storage_origin = Some(std::sync::Arc::new(storage.clone()));
    instance.source_profile = profile.to_owned();

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
    let mut workspace_info = None;
    let mut warnings: Vec<String> = Vec::new();
    let mut creation_intent = None;
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

    instance.title = final_title;
    instance.first_launch_names_agent = params.title_typed;
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
        tracing::warn!(target: "session.builder", "agent '{}' is {notice}", params.tool);
    }
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
                // Unattributed forks must still launch the parent's agent.
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
                // Seed the structured fork handshake before first connect.
                instance.view = crate::session::View::Structured;
                instance.fork_pending = Some(parent_acp_session_id);
                instance.import_pending = Some(true);
            }
        }
    }
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

            let ws_result = create_workspace(
                &primary,
                &extra_repos,
                branch,
                params.create_new_branch,
                &config.worktree.workspace_path_template,
                config.worktree.init_submodules,
                &mut instance,
            )?;

            final_path = ws_result.workspace_path.to_string_lossy().to_string();
            workspace_info = Some(ws_result.workspace_info);
            creation_intent = Some(ws_result.creation_intent);
            warnings.extend(ws_result.warnings);
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
                    let worktree_path = git_wt.compute_path(branch, template, &instance.id[..8])?;
                    anyhow::ensure!(
                        !worktree_path.try_exists()?,
                        "worktree destination already exists"
                    );
                    final_path = worktree_path.to_string_lossy().into_owned();
                    worktree_info = Some(WorktreeInfo {
                        branch: branch.clone(),
                        main_repo_path: main_repo_path.to_string_lossy().into_owned(),
                        managed_by_aoe: true,
                        created_at: Utc::now(),
                        base_branch: None,
                    });
                    instance.project_path.clone_from(&final_path);
                    instance.worktree_info.clone_from(&worktree_info);
                    creation_intent = Some(CreationIntent::reserve(storage, &mut instance)?);
                    let w = git_wt.create_worktree(branch, &worktree_path, false, None)?;
                    warnings.extend(w);
                }
            } else {
                let worktree_path = git_wt.compute_path(branch, template, &instance.id[..8])?;

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

                final_path = worktree_path.to_string_lossy().into_owned();
                worktree_info = Some(WorktreeInfo {
                    branch: branch.clone(),
                    main_repo_path: main_repo_path.to_string_lossy().into_owned(),
                    managed_by_aoe: true,
                    created_at: Utc::now(),
                    base_branch: base,
                });
                instance.project_path.clone_from(&final_path);
                instance.worktree_info.clone_from(&worktree_info);
                creation_intent = Some(CreationIntent::reserve(storage, &mut instance)?);
                let w = git_wt.create_worktree(
                    branch,
                    &worktree_path,
                    true,
                    worktree_info
                        .as_ref()
                        .and_then(|info| info.base_branch.as_deref()),
                )?;
                warnings.extend(w);
            }
        }
    }

    // For scratch sessions, `final_path` is intentionally empty here; the scratch directory is
    // provisioned below using the instance id allocated at admission.
    if !params.scratch {
        let final_path_buf = PathBuf::from(&final_path);
        if !final_path_buf.exists() {
            bail!("Project path does not exist: {}", final_path);
        }
        if !final_path_buf.is_dir() {
            bail!("Project path is not a directory: {}", final_path);
        }
    }

    instance.project_path = final_path;
    if params.scratch {
        instance.project_path = super::scratch::planned_scratch_path(&instance.id)?
            .to_string_lossy()
            .into_owned();
        instance.scratch = true;
        creation_intent = Some(CreationIntent::reserve(storage, &mut instance)?);
        super::scratch::provision_scratch_dir(&instance.id)?;
    }
    instance.worktree_info = worktree_info;
    instance.workspace_info = workspace_info;
    let creation_intent = match creation_intent {
        Some(intent) => intent,
        None => CreationIntent::reserve_metadata(storage, &mut instance)?,
    };
    creation_intent.refresh_prepared(&instance)?;

    Ok(BuildResult {
        instance,
        warnings,
        creation_intent,
    })
}

/// Complete the original filesystem intent and return the actual metadata publication.
pub(crate) fn publish_prepared_creation_under_workspace_claim_lock<F>(
    storage: &super::Storage,
    prepared: &Instance,
    intent: &CreationIntent,
    update_groups: F,
) -> Result<Instance>
where
    F: FnOnce(&mut Vec<Instance>, &mut Vec<super::Group>) -> Result<()>,
{
    anyhow::ensure!(
        storage.same_origin_as(intent.storage()),
        "creation changed its physical owner"
    );
    intent.publish_under_workspace_claim_lock(prepared, |rows, groups, committed| {
        if super::is_duplicate_session(
            rows.iter(),
            &committed.title,
            &committed.project_path,
            Some(&committed.id),
        ) {
            return Err(super::duplicate_session_error(&committed.title));
        }
        let slot = rows
            .iter_mut()
            .find(|row| row.id == committed.id)
            .ok_or_else(|| anyhow::anyhow!("original creation row disappeared"))?;
        *slot = committed;
        update_groups(rows, groups)?;
        rows.iter()
            .find(|row| row.id == prepared.id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("publication removed its original creation row"))
    })
}

/// Hold the workspace and identity fences through creation publication.
pub(crate) struct CleanupOwnershipLocks {
    _workspace_claim: crate::session::StorageFlock,
    _identity: crate::session::StorageFlock,
}

impl CleanupOwnershipLocks {
    /// Acquire in the single order every other owner uses: workspace claim
    /// before identity. Failing to acquire is an error, never a partial hold,
    /// so a caller can fail closed and retain the failed create's resources.
    pub(crate) fn acquire() -> Result<Self> {
        let workspace_claim = crate::session::acquire_session_workspace_claim_lock()?;
        let identity = crate::session::acquire_session_identity_lock()?;
        Ok(Self {
            _workspace_claim: workspace_claim,
            _identity: identity,
        })
    }
    /// Reuse the publisher's workspace and identity fences.
    pub(crate) fn from_held(
        workspace_claim: crate::session::StorageFlock,
        identity: crate::session::StorageFlock,
    ) -> Self {
        Self {
            _workspace_claim: workspace_claim,
            _identity: identity,
        }
    }
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

    fn init_repo_with_commit(name: &str) -> tempfile::TempDir {
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
        repo.commit(Some("HEAD"), &sig, &sig, "init", &tree, &[])
            .unwrap();
        parent
    }

    #[test]
    #[serial_test::serial]
    fn a_creation_intent_blocks_peer_paths_until_its_original_publication() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let peer = super::super::Storage::new_unwatched("peer").unwrap();
        std::fs::write(peer.sessions_path(), b"[]").unwrap();
        let future = home.path().join("future/worktree");
        let mut prepared = Instance::new("owned creation", future.to_str().unwrap());
        let intent = CreationIntent::reserve(&storage, &mut prepared).unwrap();
        assert!(!future.exists());
        let mut changed_plan = prepared.clone();
        changed_plan.project_path = home.path().join("another").to_string_lossy().into_owned();
        assert!(intent.publish(&changed_plan).is_err());
        assert!(storage
            .update(|rows, _| {
                rows[0].lifecycle_reservation = None;
                Ok(())
            })
            .is_err());
        let mut changed_origin = prepared.clone();
        changed_origin.storage_origin = Some(std::sync::Arc::new(peer.clone()));
        assert!(intent.publish(&changed_origin).is_err());
        let intruder = Instance::new(
            "overlapping peer",
            future.parent().unwrap().to_str().unwrap(),
        );
        assert!(peer
            .update(|rows, _| {
                rows.push(intruder.clone());
                Ok(())
            })
            .is_err());
        assert!(peer.load().unwrap().is_empty());
        assert!(storage.load().unwrap()[0].has_pending_worktree_path_claims());
        {
            let _workspace = super::super::acquire_session_workspace_claim_lock().unwrap();
            let claims = super::super::deletion::PathClaimIndex::load_for_writer(
                std::slice::from_ref(&peer),
            )
            .unwrap();
            assert!(claims.ensure_writes_unclaimed(&[future.as_path()]).is_err());
            assert!(claims
                .ensure_pending_writes_unclaimed(&[future.parent().unwrap()])
                .is_err());
            assert!(claims
                .ensure_pending_writes_unclaimed(&[future.join("child").as_path()])
                .is_err());
            assert!(claims
                .ensure_writes_unclaimed(&[home.path().join("unrelated").as_path()])
                .is_ok());
        }
        std::fs::create_dir_all(&future).unwrap();
        let peer_bytes = std::fs::read(peer.sessions_path()).unwrap();
        std::fs::write(peer.sessions_path(), b"broken ownership inventory").unwrap();
        assert!(intent.publish(&prepared).is_err());
        assert!(storage.load().unwrap()[0].has_pending_worktree_path_claims());
        std::fs::write(peer.sessions_path(), peer_bytes).unwrap();
        let committed = intent.publish(&prepared).unwrap();
        assert!(committed.lifecycle_reservation.is_none());
        let canonical = storage.load().unwrap().pop().unwrap();
        assert_eq!(canonical.project_path, future.to_str().unwrap());
        assert!(canonical.lifecycle_reservation.is_none());
        peer.update(|rows, _| {
            rows.push(intruder);
            Ok(())
        })
        .unwrap();
        assert_eq!(
            peer.load().unwrap()[0].project_path,
            future.parent().unwrap().to_str().unwrap()
        );
    }

    #[test]
    #[serial_test::serial]
    fn test_create_workspace_reports_all_concurrent_failures() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let parent_a = init_repo_with_commit("repo-a-fail");
        let parent_b = init_repo_with_commit("repo-b-fail");
        let repo_a = parent_a.path().join("repo-a-fail");
        let repo_b = parent_b.path().join("repo-b-fail");
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();
        let mut prepared = Instance::new("failed workspace", repo_a.to_str().unwrap());
        prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));

        let result = create_workspace(
            &WorkspaceRepoSpec {
                path: repo_a,
                base_branch: None,
            },
            &[WorkspaceRepoSpec {
                path: repo_b,
                base_branch: None,
            }],
            "nonexistent-branch",
            false,
            &template,
            true,
            &mut prepared,
        );

        let err = match result {
            Ok(_) => panic!("workspace creation should fail when no repo has the branch"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("repo-a-fail"),
            "first repo name missing from message: {msg}"
        );
        assert!(
            msg.contains("repo-b-fail"),
            "second repo name missing from message: {msg}"
        );
        let stored = storage
            .load()
            .unwrap()
            .into_iter()
            .find(|row| row.id == prepared.id)
            .unwrap();
        assert!(stored.has_pending_worktree_path_claims());
        assert!(matches!(stored.lifecycle_reservation.unwrap().path_claims,
            super::super::WorktreePathClaims::Pending(paths) if paths.contains(&workspaces_root.path().join("nonexistent-branch"))));
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
    #[serial_test::serial]
    fn create_workspace_honors_per_repo_base_branch() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
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
        let mut prepared = Instance::new("workspace bases", primary.to_str().unwrap());
        prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));

        let result = create_workspace(
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
            &mut prepared,
        )
        .expect("workspace creation should succeed");

        let extra_repo = result
            .workspace_info
            .repos
            .iter()
            .find(|r| r.name == "extra")
            .expect("extra repo present in workspace");
        let wt = git2::Repository::open(&extra_repo.worktree_path).unwrap();
        let head = wt.head().unwrap().peel_to_commit().unwrap();
        assert_eq!(
            head.id(),
            extra_release_tip,
            "extra repo worktree should branch from its configured `release` base"
        );
        assert_eq!(extra_repo.base_branch.as_deref(), Some("release"));
        assert_eq!(
            result
                .workspace_info
                .repos
                .iter()
                .find(|r| r.name == "primary")
                .unwrap()
                .base_branch,
            None,
            "a repo with no configured base records none, so the diff falls through to detection"
        );
    }

    #[test]
    #[serial_test::serial]
    fn create_workspace_records_no_base_when_attaching_an_existing_branch() {
        let home = tempfile::tempdir().unwrap();
        let _home_guard = crate::session::test_support::isolate_home(home.path());
        let storage = super::super::Storage::new_unwatched("default").unwrap();
        let (parent_primary, _) = init_repo_with_branch("primary", "feature-x");
        let primary = parent_primary.path().join("primary");
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();
        let mut prepared = Instance::new("existing branch workspace", primary.to_str().unwrap());
        prepared.storage_origin = Some(std::sync::Arc::new(storage.clone()));

        let result = create_workspace(
            &WorkspaceRepoSpec {
                path: primary,
                base_branch: Some("main".to_string()),
            },
            &[],
            "feature-x",
            false,
            &template,
            true,
            &mut prepared,
        )
        .expect("workspace creation should succeed");

        assert_eq!(result.workspace_info.repos[0].base_branch, None);
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
            title: format!("{tool} session"),
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
        let storage = crate::session::Storage::new_unwatched("default").unwrap();

        let result = build_instance(
            custom_agent_params(project.path(), "remote-claude"),
            &[],
            &[],
            &storage,
        )
        .unwrap();

        assert_eq!(result.instance.tool, "remote-claude");
        assert_eq!(result.instance.command, "ssh -t host claude");
        assert_eq!(result.instance.detect_as, "claude");

        let unmapped = build_instance(
            custom_agent_params(project.path(), "remote-opencode"),
            &[],
            &[],
            &storage,
        )
        .unwrap();
        assert_eq!(unmapped.instance.command, "ssh -t host opencode");
        assert_eq!(unmapped.instance.detect_as, "");

        for tool in ["remote-missing", "whitespace-agent"] {
            let Err(err) = build_instance(
                custom_agent_params(project.path(), tool),
                &[],
                &[],
                &storage,
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
        let storage = crate::session::Storage::new_unwatched("default").unwrap();

        let mut params = custom_agent_params(std::path::Path::new(""), "claude");
        params.scratch = true;
        let result = build_instance(params.clone(), &[], &[], &storage)
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
        let Err(err) = build_instance(params, &[], &[], &storage) else {
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
        let Err(err) = build_instance(params, &[], &[], &storage) else {
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
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let params = InstanceParams {
            title: "Structured fork child".into(),
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
        let inst = build_instance(params, &[], &[], &storage).unwrap().instance;
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
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
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
            title: "Terminal fork child".into(),
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
        let inst = build_instance(params, &[], &[], &storage).unwrap().instance;
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
        let storage = crate::session::Storage::new_unwatched("default").unwrap();
        let mut parent = crate::session::Instance::new("parent", root.path().to_str().unwrap());
        parent.tool = "claude".into();
        parent.agent_session_id = Some("legacy-uuid".into());
        parent.agent_session_binding =
            Some(crate::session::ConversationBinding::unknown("legacy-uuid"));
        let seed = crate::session::fork::terminal_fork_seed(
            parent.fork_parent_ref().unwrap(),
            "child-uuid".into(),
        )
        .expect("an unattributed parent is admitted");

        let mut same_agent = custom_agent_params(root.path(), "claude");
        same_agent.command_override = "claude".into();
        same_agent.fork_seed = Some(seed.clone());
        assert_eq!(
            build_instance(same_agent, &[], &[], &storage)
                .expect("a child launching the parent's own agent still forks")
                .instance
                .agent_session_id
                .as_deref(),
            Some("child-uuid")
        );

        let mut other_agent = custom_agent_params(root.path(), "codex");
        other_agent.command_override = "codex".into();
        other_agent.fork_seed = Some(seed);
        let refused = match build_instance(other_agent, &[], &[], &storage) {
            Ok(_) => panic!("a child launching another agent cannot carry the conversation"),
            Err(error) => error.to_string(),
        };
        assert!(
            refused.contains("codex") && refused.contains("claude"),
            "the refusal must name both agents: {refused}"
        );
    }
}
