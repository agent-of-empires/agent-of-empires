//! Instance creation and cleanup utilities.
//!
//! This module provides shared logic for building new session instances,
//! used by both synchronous (TUI operations) and asynchronous (background poller) code paths.

use std::{collections::HashSet, path::PathBuf};

use anyhow::{bail, Result};
use chrono::Utc;

use crate::containers;
use crate::git::error::GitError;
use crate::git::GitWorktree;

use super::{
    civilizations, Config, Instance, SandboxInfo, WorkspaceInfo, WorkspaceRepo, WorktreeInfo,
};

/// Parameters for creating a new session instance.
#[derive(Debug, Clone)]
pub struct InstanceParams {
    pub title: String,
    pub path: String,
    pub group: String,
    pub tool: String,
    pub worktree_enabled: bool,
    pub worktree_branch: Option<String>,
    pub create_new_branch: bool,
    /// Branch to base a freshly-created worktree branch on. Only honored
    /// when `create_new_branch` is true. `None` falls back to the
    /// repository's detected default branch. See #948.
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
    /// Scratch session: ignore `path`, provision a fresh directory under
    /// `<app_dir>/scratch/<id>/`, and persist `instance.scratch = true` so
    /// the deletion path removes the directory. Mutually exclusive with
    /// worktree/workspace and with non-empty `extra_repo_paths`.
    pub scratch: bool,
    /// One-shot fork seed. When `Some`, the freshly-built instance is set up
    /// to fork its parent on first launch instead of starting fresh.
    pub fork_seed: Option<crate::session::ForkSeed>,
}

/// Result of building an instance, tracking what was created for cleanup purposes.
pub struct BuildResult {
    pub instance: Instance,
    /// Path to worktree if one was created and managed by aoe
    pub created_worktree: Option<CreatedWorktree>,
    /// Workspace worktrees created during build (for cleanup)
    pub created_workspace_worktrees: Vec<CreatedWorktree>,
    /// Non-fatal warnings from worktree/workspace creation. Callers should
    /// surface these to the user (post-checkout hook failures etc.).
    pub warnings: Vec<String>,
}

/// Info about a worktree created during instance building.
pub struct CreatedWorktree {
    pub path: PathBuf,
    pub main_repo_path: PathBuf,
}

/// Result of creating a multi-repo workspace.
pub struct WorkspaceResult {
    pub workspace_info: WorkspaceInfo,
    pub created_worktrees: Vec<CreatedWorktree>,
    pub workspace_path: PathBuf,
    /// Non-fatal warnings from worktree creation (e.g. post-checkout hook
    /// failures where the worktree itself was created successfully).
    pub warnings: Vec<String>,
}

/// Normalize a base-branch string, treating empty/whitespace as unset.
fn normalize_base(s: Option<&str>) -> Option<String> {
    s.map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// Resolve a repo's effective base branch with precedence:
/// explicit session base > per-project default > global/profile default.
/// `None` means "auto-detect the repo's default branch".
pub(crate) fn resolve_base_branch(
    session: Option<&str>,
    project: Option<&str>,
    global: Option<&str>,
) -> Option<String> {
    normalize_base(session)
        .or_else(|| normalize_base(project))
        .or_else(|| normalize_base(global))
}

/// Resolve one repo's effective base branch, consulting its registered
/// per-project default. The registry stores each project at its repo root, so
/// the lookup keys on `find_main_repo(repo_path)`; `repo_path` itself may be a
/// subdirectory or a worktree, which would miss a root-keyed entry. Precedence:
/// explicit session base > per-project default > global/profile default.
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

/// Map of canonical repo path to configured default base branch for every
/// registered project (global + profile) that sets one. Used to fill in the
/// per-project layer of `resolve_repo_base_branch` for the launch repo and any
/// extra repos when building a session.
pub(crate) fn project_base_branches(profile: &str) -> std::collections::HashMap<String, String> {
    crate::session::projects::load_merged(profile)
        .unwrap_or_else(|e| {
            // Don't fork worktrees from the wrong base in silence: if the
            // registry can't be read, log it so the missing per-project
            // defaults are explainable instead of mysterious.
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

/// One repository in a multi-repo workspace, paired with the base branch its
/// freshly-created worktree branch should fork from. `base_branch` is the
/// fully resolved value (`None` means auto-detect the repo's default branch);
/// callers apply the session > per-project > global precedence before building
/// this list.
pub struct WorkspaceRepoSpec {
    pub path: PathBuf,
    pub base_branch: Option<String>,
}

/// Create a multi-repo workspace with worktrees for each repository.
///
/// Validates repo paths, detects name collisions, creates worktrees inside
/// a shared workspace directory, and rolls back on any error.
pub fn create_workspace(
    primary: &WorkspaceRepoSpec,
    extra_repos: &[WorkspaceRepoSpec],
    branch: &str,
    create_new_branch: bool,
    workspace_template: &str,
    init_submodules: bool,
) -> Result<WorkspaceResult> {
    let primary_main_repo = GitWorktree::find_main_repo(&primary.path)?;
    let primary_git_wt = GitWorktree::new(primary_main_repo)?;

    let session_id = uuid::Uuid::new_v4().to_string();
    let session_id_short = &session_id[..8];

    let workspace_path =
        primary_git_wt.compute_path(branch, workspace_template, session_id_short)?;
    let workspace_dir = workspace_path.to_string_lossy().to_string();
    std::fs::create_dir_all(&workspace_path)?;

    // (canonicalized path, resolved base branch) for the primary repo followed
    // by every extra repo. The primary path is left as the caller passed it;
    // extras are canonicalized to match how they are stored/compared.
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
            let _ = std::fs::remove_dir_all(&workspace_path);
            bail!(
                "Duplicate repository name '{}' in workspace\n\
                 Tip: Rename one of the directories to avoid the collision",
                name
            );
        }
    }

    let cleanup = |created: &[CreatedWorktree], ws_path: &std::path::Path| {
        for wt in created {
            if let Ok(git_wt) = GitWorktree::new(wt.main_repo_path.clone()) {
                let _ = git_wt.remove_worktree(&wt.path, false);
            }
        }
        let _ = std::fs::remove_dir_all(ws_path);
    };

    // Pre-validate every repo and resolve metadata sequentially. This is cheap
    // (no network) and lets us fail fast before kicking off any worktree work.
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
            cleanup(&[], &workspace_path);
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

    // Run create_worktree for every repo concurrently. Each worktree lives in
    // a different directory and uses a different main repo, so the operations
    // are independent. Network IO (git fetch + git submodule update) dominates
    // each step, so fanning out cuts wall time roughly to that of the slowest
    // repo.
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
    let mut created_worktrees: Vec<CreatedWorktree> = Vec::new();
    let mut repos: Vec<WorkspaceRepo> = Vec::with_capacity(plans.len());

    for (plan, result) in plans.iter().zip(parallel_results) {
        match result {
            Ok(w) => {
                warnings.extend(w);
                created_worktrees.push(CreatedWorktree {
                    path: plan.worktree_subdir.clone(),
                    main_repo_path: plan.main_repo_path.clone(),
                });
                repos.push(WorkspaceRepo {
                    name: plan.repo_name.clone(),
                    source_path: plan.repo_path.to_string_lossy().to_string(),
                    branch: branch.to_string(),
                    worktree_path: plan.worktree_subdir.to_string_lossy().to_string(),
                    main_repo_path: plan.main_repo_path.to_string_lossy().to_string(),
                    managed_by_aoe: true,
                });
            }
            Err(msg) => errors.push(msg),
        }
    }

    if !errors.is_empty() {
        cleanup(&created_worktrees, &workspace_path);
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
        workspace_info: WorkspaceInfo {
            branch: branch.to_string(),
            workspace_dir,
            repos,
            created_at: Utc::now(),
            cleanup_on_delete: true,
        },
        created_worktrees,
        workspace_path,
        warnings,
    })
}

/// Build an instance with all setup (worktree resolution, sandbox config).
///
/// This does NOT start the instance or create Docker containers - that happens
/// separately via `instance.start()`. This separation allows for proper cleanup
/// if starting fails.
pub fn build_instance(
    params: InstanceParams,
    existing_titles: &[&str],
    existing_branches: &[&str],
    profile: &str,
) -> Result<BuildResult> {
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

    // Scratch sessions have no project repo, so config resolution falls
    // back to global+profile defaults (`Path::new("")` makes
    // `resolve_config_with_repo` skip the repo-config layer cleanly).
    let config_path = if params.scratch {
        std::path::PathBuf::new()
    } else {
        std::path::PathBuf::from(&params.path)
    };
    let config =
        super::repo_config::resolve_config_with_repo(profile, &config_path).unwrap_or_else(|e| {
            tracing::warn!(target: "session.create", "Failed to load config, using defaults: {}", e);
            Config::default()
        });

    let mut final_path = if params.scratch {
        // Provisioning happens after `Instance::new` so we can key the
        // directory on the generated instance id. Leave `final_path` empty
        // for now; the worktree/workspace and path-existence blocks below
        // are gated on the same flag.
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

    if let Some(branch) = &effective_worktree_branch {
        if !params.extra_repo_paths.is_empty() {
            let primary_path = PathBuf::from(&params.path)
                .canonicalize()
                .unwrap_or_else(|_| PathBuf::from(&params.path));

            let session_base = params.base_branch.as_deref();
            let global_default = config.worktree.default_base_branch.as_deref();
            let project_bases = project_base_branches(profile);

            // Every repo, including the launch repo, forks from its own
            // registered per-project default when no explicit session base is
            // given. Keyed by repo root so a launch path inside a subdirectory
            // still matches a root-registered project.
            let primary = WorkspaceRepoSpec {
                base_branch: resolve_repo_base_branch(
                    &primary_path,
                    session_base,
                    &project_bases,
                    global_default,
                ),
                path: primary_path,
            };
            let extra_repos: Vec<WorkspaceRepoSpec> = params
                .extra_repo_paths
                .iter()
                .map(|p| {
                    let path = PathBuf::from(p);
                    WorkspaceRepoSpec {
                        base_branch: resolve_repo_base_branch(
                            &path,
                            session_base,
                            &project_bases,
                            global_default,
                        ),
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
            )?;

            final_path = ws_result.workspace_path.to_string_lossy().to_string();
            workspace_info = Some(ws_result.workspace_info);
            created_workspace_worktrees = ws_result.created_worktrees;
            warnings.extend(ws_result.warnings);
        } else {
            // Single worktree mode (existing logic)
            let path = PathBuf::from(&params.path);
            if !GitWorktree::is_git_repo(&path) {
                // Typed error (not a bare `bail!` string) so the web handler's
                // whitelist forwards an actionable message instead of the
                // opaque "Failed to create session". The context keeps the
                // fuller tip for callers that surface the anyhow chain (CLI,
                // TUI).
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
                    let session_id = uuid::Uuid::new_v4().to_string();
                    let worktree_path = git_wt.compute_path(branch, template, &session_id[..8])?;

                    let w = git_wt.create_worktree(branch, &worktree_path, false, None)?;
                    warnings.extend(w);

                    final_path = worktree_path.to_string_lossy().to_string();
                    created_worktree = Some(CreatedWorktree {
                        path: worktree_path,
                        main_repo_path: main_repo_path.clone(),
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
                let session_id = uuid::Uuid::new_v4().to_string();
                let worktree_path = git_wt.compute_path(branch, template, &session_id[..8])?;

                if worktree_path.exists() {
                    return Err(GitError::WorktreeAlreadyExists(worktree_path.clone()).into());
                }

                // The launch repo forks from its registered per-project default
                // when no explicit session base is given (then global/profile,
                // then auto-detect). Keyed by repo root via the shared helper.
                let project_bases = project_base_branches(profile);
                let base = resolve_repo_base_branch(
                    &main_repo_path,
                    params.base_branch.as_deref(),
                    &project_bases,
                    config.worktree.default_base_branch.as_deref(),
                );

                let w = git_wt.create_worktree(branch, &worktree_path, true, base.as_deref())?;
                warnings.extend(w);

                final_path = worktree_path.to_string_lossy().to_string();
                created_worktree = Some(CreatedWorktree {
                    path: worktree_path,
                    main_repo_path: main_repo_path.clone(),
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

    // For scratch sessions, `final_path` is intentionally empty here; the
    // scratch directory is provisioned below after `Instance::new` runs (we
    // need the instance id to name the directory). For all other sessions,
    // catch the typed-a-bad-path case before tmux silently falls back to
    // the home directory.
    if !params.scratch {
        let final_path_buf = PathBuf::from(&final_path);
        if !final_path_buf.exists() {
            bail!("Project path does not exist: {}", final_path);
        }
        if !final_path_buf.is_dir() {
            bail!("Project path is not a directory: {}", final_path);
        }
    }

    let mut instance = Instance::new(&final_title, &final_path);
    if params.scratch {
        let dir = super::scratch::provision_scratch_dir(&instance.id)?;
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
    instance.worktree_info = worktree_info;
    instance.workspace_info = workspace_info;
    instance.yolo_mode = params.yolo_mode;

    // Apply command overrides and custom agent commands from resolved config.
    // Priority: per-session params > agent_command_override > custom_agents > AgentDef default.
    if !params.command_override.is_empty() {
        instance.command = params.command_override;
    } else {
        let resolved = config.session.resolve_tool_command(&params.tool);
        if !resolved.is_empty() {
            instance.command = resolved;
        }
    }
    if instance.command.trim().is_empty() && crate::agents::get_agent(&params.tool).is_none() {
        bail!(
            "No launch command resolved for custom agent '{}'. Config may have changed since validation.",
            params.tool
        );
    }
    if !params.extra_args.is_empty() {
        instance.extra_args = params.extra_args;
    } else if let Some(extra) = config.session.agent_extra_args.get(&params.tool) {
        if !extra.is_empty() {
            instance.extra_args = extra.clone();
        }
    }

    if params.sandbox {
        // Surface env-resolution warnings up-front. `collect_environment`
        // silently drops entries whose host source var is unset (typo,
        // shell sourcing gap, daemon's frozen env). Without this check
        // the value is missing in the container with no UI signal.
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
            container_workdir: None,
        });
    }

    if let Some(seed) = params.fork_seed {
        match seed {
            crate::session::ForkSeed::Terminal {
                parent_agent_session_id,
                child_session_id,
            } => {
                // Pre-pin the child id so it is durable on disk before launch,
                // and carry the parent on the one-shot Fork intent.
                instance.agent_session_id = Some(child_session_id);
                instance.resume_intent = crate::session::ResumeIntent::Fork {
                    from: parent_agent_session_id,
                };
            }
            crate::session::ForkSeed::Structured {
                parent_acp_session_id,
            } => {
                // Structured fork: force the structured view, seed the parent
                // for the ACP session/fork handshake, and replay history into
                // the (empty) event store on first connect. The marker fields
                // live behind the serve feature, so without it a structured
                // fork is inapplicable and this arm is a no-op. Bind the field
                // to `_` on bare-core so the destructure reads it without an
                // `allow(unused_variables)` suppression (AGENTS.md).
                #[cfg(feature = "serve")]
                {
                    instance.view = crate::session::View::Structured;
                    instance.fork_pending = Some(parent_acp_session_id);
                    instance.import_pending = Some(true);
                }
                #[cfg(not(feature = "serve"))]
                let _ = parent_acp_session_id;
            }
        }
    }

    Ok(BuildResult {
        instance,
        created_worktree,
        created_workspace_worktrees,
        warnings,
    })
}

/// Clean up resources created during a failed or cancelled instance build.
///
/// This handles:
/// - Removing worktrees created by aoe
/// - Removing Docker containers
/// - Killing tmux sessions
pub fn cleanup_instance(
    instance: &Instance,
    created_worktree: Option<&CreatedWorktree>,
    created_workspace_worktrees: &[CreatedWorktree],
) {
    // Scratch dirs are provisioned eagerly inside `build_instance`
    // (well before this helper's other cleanup targets exist), so an
    // abort between provisioning and the caller finishing the session
    // would otherwise leak the directory on disk. Guard the removal
    // through `is_scratch_path` so a tampered `project_path` cannot
    // wipe unrelated app data.
    if instance.scratch {
        let scratch_path = PathBuf::from(&instance.project_path);
        if super::scratch::is_scratch_path(&scratch_path) {
            if let Err(e) = std::fs::remove_dir_all(&scratch_path) {
                tracing::warn!(
                    target: "session.create",
                    "Failed to clean up scratch dir: {}",
                    e
                );
            }
        }
    }

    if let Some(wt) = created_worktree {
        if let Ok(git_wt) = GitWorktree::new(wt.main_repo_path.clone()) {
            if let Err(e) = git_wt.remove_worktree(&wt.path, false) {
                tracing::warn!(target: "session.create", "Failed to clean up worktree: {}", e);
            }
        }
    }

    // Workspace worktree cleanup
    for wt in created_workspace_worktrees {
        if let Ok(git_wt) = GitWorktree::new(wt.main_repo_path.clone()) {
            if let Err(e) = git_wt.remove_worktree(&wt.path, false) {
                tracing::warn!(target: "session.create", "Failed to clean up workspace worktree: {}", e);
            }
        }
    }
    // Clean up workspace directory if workspace was created
    if let Some(ws_info) = &instance.workspace_info {
        let _ = std::fs::remove_dir_all(&ws_info.workspace_dir);
    }

    if let Some(sandbox) = &instance.sandbox_info {
        if sandbox.enabled {
            // Direct idempotent teardown, never gated on a separate existence
            // probe: a transient `inspect` failure must not skip removal and
            // orphan a live container. Volumes are swept inside `teardown`.
            let container = containers::DockerContainer::from_session_id(&instance.id);
            if let containers::Teardown::Failed(e) = container.teardown(&instance.id) {
                tracing::warn!(target: "session.create", "Failed to clean up container: {}", e);
            }
        }
    }

    let _ = instance.kill();
}

/// Structured-view (ACP) helpers for the TUI create paths. The web create
/// path does the equivalent inline in `src/server/api/sessions.rs` (it also
/// handles explicit agent / model / import fields the TUI wizard doesn't
/// expose), and the CLI in `src/cli/add.rs` with bail-vs-downgrade semantics
/// keyed on how explicit the user's flag was. Keep the three in sync.
#[cfg(feature = "serve")]
pub mod structured {
    use super::Instance;

    /// True when `tool` can back a structured-view session: it resolves in
    /// the ACP agent registry, or the resolved config declares a parsable
    /// `[session.agent_acp_cmd]` command for it. Mirrors the server create
    /// path's capability re-validation; deliberately NOT the aoe-agent
    /// fallback (`pick_acp_agent_name`), which would make every tool look
    /// capable.
    pub fn tool_acp_capable(tool: &str, config: &crate::session::Config) -> bool {
        crate::acp::agent_registry::AgentRegistry::with_defaults()
            .get(tool)
            .is_some()
            || config
                .session
                .agent_acp_cmd
                .get(tool)
                .is_some_and(|cmd| crate::acp::AgentSpec::from_acp_cmd(tool, cmd).is_ok())
    }

    /// Pre-create validation for an explicit structured-view choice from the
    /// new-session wizard, run BEFORE any worktree / scratch / container is
    /// provisioned so a refusal can't orphan resources (same ordering as the
    /// CLI's precondition). Returns a user-facing message on refusal.
    ///
    /// The adapter-on-PATH check only runs when the user has no command
    /// override for the tool: an override swaps the binary the spawn will
    /// actually exec (see #1910), and second-guessing it here could refuse a
    /// working setup. With an override, a genuinely missing adapter surfaces
    /// as the structured view's startup-error banner instead.
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
                None => unreachable!("tool_acp_capable implies a resolvable spec"),
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

    /// Apply a validated structured-view choice to a freshly-built instance:
    /// set the persisted view and pin the per-agent default model, the same
    /// post-build step the web create handler runs. Re-validates capability
    /// defensively (downgrading to terminal with a warning) so a caller that
    /// skipped [`validate_structured_choice`] can't persist a structured
    /// session no agent can serve.
    pub fn apply_structured_choice(instance: &mut Instance) {
        let config = crate::session::repo_config::resolve_config_with_repo_or_warn(
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
        // Pin the per-agent default model so the composer shows it and the
        // session stays on it (mirrors the CLI and web create paths). The
        // wizard sets no explicit model, so the default is the only input.
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

/// Origin of an effective worktree branch name. The builder uses this to decide
/// whether collisions with existing branches should be resolved by suffixing
/// (Derived) or surfaced as an error (Explicit).
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
            // Defense-in-depth: even if the frontend slug missed a forbidden
            // char (or the caller is a CLI/API user typing a title-shaped
            // string into the branch field), sanitise here so libgit2 never
            // sees a value it'll reject with InvalidSpec. `/` is preserved
            // since it's the legal namespace separator in git refs.
            Some(b) => BranchSource::Explicit(git_sanitize_branch_name(b)),
            None => BranchSource::Derived(branch_name_from_title(final_title)),
        },
    )
}

/// Replace characters that git ref names cannot contain (per
/// `git-check-ref-format(1)`) with '-'. Unlike `branch_name_from_title`
/// this keeps the user's casing and preserves '/' so `feat/auth`-style
/// branches survive when the user types them explicitly.
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
    // Strip the ".lock" suffix from every slash-separated component, not
    // just the last one; git-check-ref-format(1) rejects any component
    // ending in ".lock" (e.g. `foo.lock/bar` is just as invalid as
    // `foo.lock`).
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
/// If `base` is free, returns it unchanged. Otherwise appends `-2`, `-3`, …
/// until a free name is found.
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
/// NFKD decomposition handles accented characters (é → e + combining acute, then
/// the combining mark is dropped by the ASCII filter), but ligatures and stroked
/// letters have no canonical decomposition, so we expand them here.
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
        // Preserve '/' as git's namespace separator (so a title like
        // `jacob/feature-1` yields a branch `jacob/feature-1`). The worktree
        // folder leaf is sanitized separately via `sanitize_branch_name`, so
        // the slash never reaches a path. Never emit a leading, trailing, or
        // doubled slash; trim any pending dash before it.
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
    fn test_empty_title_with_worktree_uses_branch_name() {
        let title = resolve_title("", Some("feature-auth"), true, &[], &HashSet::new()).unwrap();
        assert_eq!(title, "feature-auth");
    }

    #[test]
    fn test_empty_title_without_worktree_uses_civilization() {
        let title = resolve_title("", None, false, &[], &HashSet::new()).unwrap();
        assert!(
            civilizations::CIVILIZATIONS.contains(&title.as_str()),
            "Expected a civilization name, got: {}",
            title
        );
    }

    #[test]
    fn test_provided_title_with_worktree_keeps_title() {
        let title = resolve_title(
            "My Session",
            Some("feature-auth"),
            true,
            &[],
            &HashSet::new(),
        )
        .unwrap();
        assert_eq!(title, "My Session");
    }

    #[test]
    fn test_provided_title_without_worktree_keeps_title() {
        let title = resolve_title("Custom Name", None, false, &[], &HashSet::new()).unwrap();
        assert_eq!(title, "Custom Name");
    }

    #[test]
    fn test_empty_worktree_title_skips_civ_with_taken_branch() {
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
    fn test_worktree_branch_derived_from_title_when_name_empty() {
        let branch = resolve_worktree_branch(true, None, "Fix Login Flow").unwrap();
        assert!(matches!(branch, BranchSource::Derived(ref s) if s == "fix-login-flow"));
    }

    #[test]
    fn test_worktree_branch_preserves_explicit_name() {
        // The git-safe sanitiser leaves valid refs alone: '/' is a legal
        // namespace separator, so `feat/auth` survives unchanged.
        let branch = resolve_worktree_branch(true, Some("feat/auth"), "Fix Login Flow").unwrap();
        assert!(matches!(branch, BranchSource::Explicit(ref s) if s == "feat/auth"));
    }

    #[test]
    fn test_worktree_branch_sanitizes_explicit_with_spaces() {
        // Without this, the value reaches libgit2 and surfaces as the opaque
        // 'reference name … is not valid' InvalidSpec error in the dashboard.
        let branch =
            resolve_worktree_branch(true, Some("Exploration and issues v2"), "Fix Login Flow")
                .unwrap();
        assert!(
            matches!(branch, BranchSource::Explicit(ref s) if s == "Exploration-and-issues-v2")
        );
    }

    #[test]
    fn test_git_sanitize_branch_name_passes_through_valid_refs() {
        assert_eq!(git_sanitize_branch_name("feat/auth"), "feat/auth");
        assert_eq!(git_sanitize_branch_name("release-1.2.3"), "release-1.2.3");
        assert_eq!(
            git_sanitize_branch_name("user_name/topic"),
            "user_name/topic"
        );
    }

    #[test]
    fn test_git_sanitize_branch_name_replaces_forbidden_chars() {
        assert_eq!(git_sanitize_branch_name("has spaces"), "has-spaces");
        assert_eq!(git_sanitize_branch_name("a:b?c*d"), "a-b-c-d");
        assert_eq!(git_sanitize_branch_name("ref^name"), "ref-name");
        assert_eq!(git_sanitize_branch_name("a..b"), "a-b");
        assert_eq!(git_sanitize_branch_name("a@{b"), "a-b");
    }

    #[test]
    fn test_git_sanitize_branch_name_trims_edges() {
        assert_eq!(git_sanitize_branch_name("  hello  "), "hello");
        assert_eq!(git_sanitize_branch_name("-leading"), "leading");
        assert_eq!(git_sanitize_branch_name(".hidden"), "hidden");
        assert_eq!(git_sanitize_branch_name("/foo"), "foo");
        assert_eq!(git_sanitize_branch_name("foo/"), "foo");
        assert_eq!(git_sanitize_branch_name("foo.lock"), "foo");
        assert_eq!(git_sanitize_branch_name(""), "session");
    }

    #[test]
    fn test_git_sanitize_branch_name_strips_interior_lock_suffix() {
        // git-check-ref-format rejects ANY slash-separated component ending
        // in ".lock", not just the trailing one.
        assert_eq!(git_sanitize_branch_name("foo.lock/bar"), "foo/bar");
        assert_eq!(
            git_sanitize_branch_name("feat/release.lock/v2"),
            "feat/release/v2"
        );
        assert_eq!(git_sanitize_branch_name("foo.lock.lock"), "foo");
        assert_eq!(
            git_sanitize_branch_name("feat/release.lock.lock/v2.lock.lock"),
            "feat/release/v2"
        );
    }

    #[test]
    fn test_git_sanitize_branch_name_rejects_special_complete_refs() {
        // git-check-ref-format also rejects special complete ref names; fall
        // back to "session" rather than producing a name libgit2 will refuse.
        assert_eq!(git_sanitize_branch_name("@"), "session");
        assert_eq!(git_sanitize_branch_name("HEAD"), "session");
    }

    #[test]
    fn test_worktree_branch_disabled_without_worktree() {
        assert!(resolve_worktree_branch(false, Some("feat/auth"), "Fix Login Flow").is_none());
    }

    #[test]
    fn test_branch_name_from_title_sanitizes_git_hostile_chars() {
        assert_eq!(
            branch_name_from_title("Fix: login @ mobile #42"),
            "fix-login-mobile-42"
        );
        // '/' is the legal git namespace separator and is preserved; '.' is
        // still folded to '-'.
        assert_eq!(
            branch_name_from_title("feat/auth.refactor"),
            "feat/auth-refactor"
        );
    }

    #[test]
    fn test_branch_name_from_title_preserves_slashes() {
        // The motivating case: a `user/topic` title keeps its slash in the
        // branch, while the worktree folder leaf is sanitized elsewhere.
        assert_eq!(branch_name_from_title("jacob/feature-1"), "jacob/feature-1");
        // No leading, trailing, or doubled slash; dashes around a slash are
        // trimmed.
        assert_eq!(branch_name_from_title("/leading"), "leading");
        assert_eq!(branch_name_from_title("trailing/"), "trailing");
        assert_eq!(branch_name_from_title("a//b"), "a/b");
        assert_eq!(branch_name_from_title("a / b"), "a/b");
    }

    #[test]
    fn test_branch_name_from_title_folds_latin_diacritics() {
        assert_eq!(branch_name_from_title("café fix"), "cafe-fix");
        assert_eq!(branch_name_from_title("naïve solution"), "naive-solution");
        assert_eq!(branch_name_from_title("Straße"), "strasse");
        assert_eq!(branch_name_from_title("Łódź"), "lodz");
        assert_eq!(branch_name_from_title("crème brûlée"), "creme-brulee");
        assert_eq!(branch_name_from_title("œuvre"), "oeuvre");
    }

    #[test]
    fn test_branch_name_from_title_drops_unsupported_scripts() {
        // CJK and emoji are not in the Latin transliteration table, so they're
        // stripped (current best-effort behavior). The "session" fallback kicks in
        // when nothing usable remains.
        assert_eq!(branch_name_from_title("测试"), "session");
        assert_eq!(branch_name_from_title("🚀 ship"), "ship");
    }

    #[test]
    fn test_dedupe_branch_name_returns_base_when_free() {
        let taken = std::collections::HashSet::new();
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug");
    }

    #[test]
    fn test_dedupe_branch_name_appends_suffix_on_collision() {
        let mut taken = HashSet::new();
        taken.insert("fix-bug".to_string());
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-2");

        taken.insert("fix-bug-2".to_string());
        taken.insert("fix-bug-3".to_string());
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-4");
    }

    #[test]
    fn test_dedupe_branch_name_matches_case_insensitively() {
        let mut taken = HashSet::new();
        taken.insert("Tatars".to_string());

        assert_eq!(dedupe_branch_name("tatars", &taken), "tatars-2");
    }

    /// Init a non-bare repo named `name` inside its own TempDir with one
    /// commit. Returns the TempDir (path is the repo root).
    fn init_repo_with_commit(name: &str) -> tempfile::TempDir {
        // We want the directory's file_name to be `name` so the parallel
        // error message references it. TempDir uses random suffixes, so we
        // create a wrapping TempDir and then a known-named subdir inside it
        // by leveraging tempfile::Builder.
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
    fn test_create_workspace_reports_all_concurrent_failures() {
        // Two repos that each only have a "main"/"master" branch. Asking for
        // a non-existent branch with create_new_branch=false makes both
        // create_worktree calls fail in parallel; the bail! message must
        // include both repo names.
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
        );

        let err = match result {
            Ok(_) => panic!("workspace creation should fail when no repo has the branch"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("Failed to create worktrees"),
            "multi-error bail! prefix missing: {msg}"
        );
        assert!(msg.contains("(2 repos)"), "should report repo count: {msg}");
        assert!(
            msg.contains("repo-a-fail"),
            "first repo name missing from message: {msg}"
        );
        assert!(
            msg.contains("repo-b-fail"),
            "second repo name missing from message: {msg}"
        );
    }

    #[test]
    fn test_create_workspace_single_failure_keeps_simple_message() {
        // One bad repo; the message should NOT use the multi-error format
        // (no "(N repos):" prefix) and SHOULD use the singular phrasing.
        let parent_a = init_repo_with_commit("repo-solo-fail");
        let repo_a = parent_a.path().join("repo-solo-fail");
        let workspaces_root = tempfile::TempDir::new().unwrap();
        let template = workspaces_root
            .path()
            .join("{branch}")
            .to_string_lossy()
            .into_owned();

        let result = create_workspace(
            &WorkspaceRepoSpec {
                path: repo_a,
                base_branch: None,
            },
            &[],
            "nonexistent-branch",
            false,
            &template,
            true,
        );

        let err = match result {
            Ok(_) => panic!("single-repo failure should still surface"),
            Err(e) => e,
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("Failed to create worktree for"),
            "singular phrasing missing: {msg}"
        );
        assert!(
            !msg.contains("repos):"),
            "single-failure path should not use multi-error wording: {msg}"
        );
        assert!(msg.contains("repo-solo-fail"), "repo name missing: {msg}");
    }

    #[test]
    fn resolve_base_branch_precedence() {
        // Explicit session base wins over everything.
        assert_eq!(
            resolve_base_branch(Some("session"), Some("project"), Some("global")),
            Some("session".to_string())
        );
        // Per-project default fills in when there is no session base.
        assert_eq!(
            resolve_base_branch(None, Some("project"), Some("global")),
            Some("project".to_string())
        );
        // Global/profile default is the last configured layer.
        assert_eq!(
            resolve_base_branch(None, None, Some("global")),
            Some("global".to_string())
        );
        // Nothing set means auto-detect.
        assert_eq!(resolve_base_branch(None, None, None), None);
        // Empty/whitespace at any layer is treated as unset and skipped.
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

        // No explicit session base: the launch repo forks from its registered
        // per-project default.
        assert_eq!(
            resolve_repo_base_branch(&root, None, &bases, Some("global")),
            Some("develop".to_string())
        );

        // Explicit session base still wins over the per-project default.
        assert_eq!(
            resolve_repo_base_branch(&root, Some("hotfix"), &bases, Some("global")),
            Some("hotfix".to_string())
        );

        // No registered entry for this repo: fall back to the global default.
        let empty = std::collections::HashMap::new();
        assert_eq!(
            resolve_repo_base_branch(&root, None, &empty, Some("global")),
            Some("global".to_string())
        );
    }

    #[test]
    fn resolve_repo_base_branch_matches_when_launching_from_a_worktree() {
        // A session launched from a linked worktree of the registered repo
        // still resolves the project default, because find_main_repo maps the
        // worktree back to its main repo (the registry's key).
        let (parent, _tip) = init_repo_with_branch("proj", "release");
        let root = parent.path().join("proj");
        let main_wt = GitWorktree::new(root.clone()).unwrap();
        let wt_path = parent.path().join("proj-wt");
        main_wt
            .create_worktree("wt-branch", &wt_path, true, None)
            .unwrap();

        let key = crate::session::projects::canonical_key(&root.to_string_lossy());
        let mut bases = std::collections::HashMap::new();
        bases.insert(key, "develop".to_string());

        assert_eq!(
            resolve_repo_base_branch(&wt_path, None, &bases, None),
            Some("develop".to_string())
        );
    }

    /// Create a repo with `main` plus a second branch holding a distinct
    /// commit. Returns the parent TempDir and the second branch's tip oid so a
    /// test can assert a worktree forked from it instead of `main`.
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

        // Branch off and add a distinct commit so the tip differs from `main`.
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
        // The extra repo's worktree should fork from its own configured base
        // branch, while the primary repo forks from its default branch.
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
    }

    fn isolated_app_dir(temp_home: &std::path::Path) -> std::path::PathBuf {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            let config_home = temp_home.join(".config");
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            config_home.join(crate::session::APP_DIR_NAME_XDG)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            temp_home.join(crate::session::APP_DIR_NAME_OTHER)
        }
    }

    fn custom_agent_params(project_path: &std::path::Path, tool: &str) -> InstanceParams {
        InstanceParams {
            title: "custom session".to_string(),
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
            scratch: false,
            fork_seed: None,
        }
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_preserves_custom_agent_detect_as_mapping() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                remote-claude = "ssh -t host claude"

                [session.agent_detect_as]
                remote-claude = "claude"
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();

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
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_keeps_empty_detect_as_without_mapping() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                remote-opencode = "ssh -t host opencode"
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();

        let result = build_instance(
            custom_agent_params(project.path(), "remote-opencode"),
            &[],
            &[],
            "default",
        )
        .unwrap();

        assert_eq!(result.instance.tool, "remote-opencode");
        assert_eq!(result.instance.command, "ssh -t host opencode");
        assert_eq!(result.instance.detect_as, "");
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_rejects_custom_agent_without_resolved_command() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), "").unwrap();
        let project = tempfile::tempdir().unwrap();

        let result = build_instance(
            custom_agent_params(project.path(), "remote-missing"),
            &[],
            &[],
            "default",
        );
        let err = match result {
            Ok(_) => panic!("custom agent without a command should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("No launch command resolved for custom agent 'remote-missing'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_rejects_custom_agent_with_whitespace_only_command() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(
            app_dir.join("config.toml"),
            r#"
                [session.custom_agents]
                whitespace-agent = "   "
            "#,
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();

        let result = build_instance(
            custom_agent_params(project.path(), "whitespace-agent"),
            &[],
            &[],
            "default",
        );
        let err = match result {
            Ok(_) => panic!("custom agent with whitespace-only command should fail"),
            Err(err) => err,
        };

        assert!(
            err.to_string()
                .contains("No launch command resolved for custom agent 'whitespace-agent'"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_scratch_provisions_app_dir() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), "").unwrap();

        let mut params = custom_agent_params(std::path::Path::new(""), "claude");
        params.tool = "claude".to_string();
        params.path = String::new();
        params.scratch = true;
        params.sandbox = false;

        let result = build_instance(params, &[], &[], "default")
            .expect("scratch build must succeed without a project path");

        assert!(
            result.instance.scratch,
            "scratch flag must be persisted on the instance"
        );
        let provisioned = std::path::PathBuf::from(&result.instance.project_path);
        assert!(provisioned.exists());
        assert!(super::super::scratch::is_scratch_path(&provisioned));

        let _ = std::fs::remove_dir_all(&provisioned);
    }

    #[test]
    fn build_instance_applies_terminal_fork_seed() {
        use crate::session::ForkSeed;
        let params = InstanceParams {
            title: "Forked".into(),
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
            scratch: false,
            fork_seed: Some(ForkSeed::Terminal {
                parent_agent_session_id: "parent-uuid".into(),
                child_session_id: "child-uuid".into(),
            }),
        };
        let inst = build_instance(params, &[], &[], "default")
            .unwrap()
            .instance;
        // The pre-pinned child id lives in agent_session_id; the parent rides on
        // the one-shot Fork intent.
        assert_eq!(inst.agent_session_id.as_deref(), Some("child-uuid"));
        assert!(
            matches!(
                inst.resume_intent,
                crate::session::instance::ResumeIntent::Fork { ref from } if from == "parent-uuid"
            ),
            "fork intent must carry the parent id in `from`, got {:?}",
            inst.resume_intent
        );
    }

    #[cfg(feature = "serve")]
    #[test]
    fn build_instance_applies_structured_fork_seed() {
        use crate::session::ForkSeed;
        let params = InstanceParams {
            title: "Forked".into(),
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
            scratch: false,
            fork_seed: Some(ForkSeed::Structured {
                parent_acp_session_id: "parent-acp-id".into(),
            }),
        };
        let inst = build_instance(params, &[], &[], "default")
            .unwrap()
            .instance;
        // The structured arm forces the structured view and sets the two paired
        // one-shot markers: fork_pending carries the parent for session/fork,
        // and import_pending replays history into the fresh event store. A
        // regression on any of the three should fail here, not only in the
        // aggregate structured e2e.
        assert_eq!(inst.view, crate::session::View::Structured);
        assert_eq!(inst.fork_pending.as_deref(), Some("parent-acp-id"));
        assert_eq!(inst.import_pending, Some(true));
        // Structured fork does not pre-pin an agent id (the adapter mints the
        // child id at handshake) and leaves the terminal Fork intent unset.
        assert!(inst.agent_session_id.is_none());
        assert!(!matches!(
            inst.resume_intent,
            crate::session::instance::ResumeIntent::Fork { .. }
        ));
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_rejects_scratch_with_worktree() {
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), "").unwrap();

        let mut params = custom_agent_params(std::path::Path::new(""), "claude");
        params.tool = "claude".to_string();
        params.scratch = true;
        params.worktree_enabled = true;
        params.worktree_branch = Some("feat".to_string());

        let err = match build_instance(params, &[], &[], "default") {
            Ok(_) => panic!("scratch + worktree must error"),
            Err(e) => e,
        };
        assert!(
            err.to_string()
                .contains("Cannot combine --scratch with worktree mode"),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn build_instance_worktree_on_non_git_path_returns_typed_not_a_git_repo() {
        // A worktree requested on a plain (non-git) folder must fail with the
        // typed GitError::NotAGitRepo, not a bare `bail!` string. The web
        // handler only forwards an actionable message for whitelisted typed
        // GitError variants, so a string bail would surface the opaque
        // "Failed to create session" instead.
        let temp_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", temp_home.path());
        let app_dir = isolated_app_dir(temp_home.path());
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(app_dir.join("config.toml"), "").unwrap();

        let project = tempfile::tempdir().unwrap();
        let mut params = custom_agent_params(project.path(), "claude");
        params.tool = "claude".to_string();
        params.worktree_enabled = true;
        params.worktree_branch = Some("feat".to_string());

        let err = match build_instance(params, &[], &[], "default") {
            Ok(_) => panic!("worktree on a non-git path must error"),
            Err(e) => e,
        };
        assert!(
            err.chain()
                .filter_map(|c| c.downcast_ref::<crate::git::error::GitError>())
                .any(|g| matches!(g, crate::git::error::GitError::NotAGitRepo)),
            "expected a typed GitError::NotAGitRepo in the chain, got: {err:#}"
        );
    }
}
