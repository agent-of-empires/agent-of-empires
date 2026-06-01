//! Instance creation and cleanup utilities.
//!
//! This module provides shared logic for building new session instances,
//! used by both synchronous (TUI operations) and asynchronous (background poller) code paths.

use std::path::PathBuf;

use anyhow::{bail, Result};
use chrono::Utc;

use crate::containers::{self, ContainerRuntimeInterface};
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

/// Create a multi-repo workspace with worktrees for each repository.
///
/// Validates repo paths, detects name collisions, creates worktrees inside
/// a shared workspace directory, and rolls back on any error.
pub fn create_workspace(
    primary_path: &std::path::Path,
    extra_repo_paths: &[PathBuf],
    branch: &str,
    create_new_branch: bool,
    base_branch: Option<&str>,
    workspace_template: &str,
    init_submodules: bool,
) -> Result<WorkspaceResult> {
    let primary_main_repo = GitWorktree::find_main_repo(primary_path)?;
    let primary_git_wt = GitWorktree::new(primary_main_repo)?;

    let session_id = uuid::Uuid::new_v4().to_string();
    let session_id_short = &session_id[..8];

    let workspace_path =
        primary_git_wt.compute_path(branch, workspace_template, session_id_short)?;
    let workspace_dir = workspace_path.to_string_lossy().to_string();
    std::fs::create_dir_all(&workspace_path)?;

    let all_repo_paths: Vec<PathBuf> = std::iter::once(primary_path.to_path_buf())
        .chain(
            extra_repo_paths
                .iter()
                .map(|r| r.canonicalize().unwrap_or_else(|_| r.clone())),
        )
        .collect();

    // Check for duplicate repo directory names
    let mut seen_names = std::collections::HashSet::new();
    for repo_path in &all_repo_paths {
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
    }
    let mut plans: Vec<RepoPlan> = Vec::with_capacity(all_repo_paths.len());
    for repo_path in &all_repo_paths {
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
                    let base = base_branch.map(|b| b.to_string());
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
    let final_title = resolve_title(
        &params.title,
        params.worktree_branch.as_deref(),
        params.worktree_enabled,
        existing_titles,
    );
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
                let mut taken: std::collections::HashSet<String> =
                    existing_branches.iter().map(|s| (*s).to_string()).collect();
                if let Ok(local) =
                    crate::git::diff::list_branches(std::path::Path::new(&params.path))
                {
                    taken.extend(local);
                }
                Some(dedupe_branch_name(&name, &taken))
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
            let extra_paths: Vec<PathBuf> =
                params.extra_repo_paths.iter().map(PathBuf::from).collect();

            let ws_result = create_workspace(
                &primary_path,
                &extra_paths,
                branch,
                params.create_new_branch,
                params.base_branch.as_deref(),
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
                bail!("Path is not in a git repository");
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

                let w = git_wt.create_worktree(
                    branch,
                    &worktree_path,
                    true,
                    params.base_branch.as_deref(),
                )?;
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
                    base_branch: params.base_branch.clone(),
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
        });
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
            let container = containers::DockerContainer::from_session_id(&instance.id);
            if container.exists().unwrap_or(false) {
                if let Err(e) = container.remove(true) {
                    tracing::warn!(target: "session.create", "Failed to clean up container: {}", e);
                }
            }
            // Remove named ignore volumes even if the container is already gone.
            container.remove_named_ignore_volumes(&instance.id);
        }
    }

    let _ = instance.kill();
}

/// Resolve the session title: use the provided title, then an explicit worktree
/// branch name, then fall back to a random civilization name.
pub(crate) fn resolve_title(
    title: &str,
    worktree_branch: Option<&str>,
    worktree_enabled: bool,
    existing_titles: &[&str],
) -> String {
    if title.is_empty() {
        if worktree_enabled {
            if let Some(branch) = worktree_branch.filter(|b| !b.trim().is_empty()) {
                branch.trim().to_string()
            } else {
                civilizations::generate_random_title(existing_titles)
            }
        } else {
            civilizations::generate_random_title(existing_titles)
        }
    } else {
        title.to_string()
    }
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
        .map(|seg| seg.strip_suffix(".lock").unwrap_or(seg))
        .collect::<Vec<_>>()
        .join("/");
    while matches!(out.chars().last(), Some('-' | '.' | '/')) {
        out.pop();
    }
    while matches!(out.chars().next(), Some('-' | '.' | '/')) {
        out.remove(0);
    }
    // A lone '@' is also rejected by git as a complete ref name.
    if out.is_empty() || out == "@" {
        "session".to_string()
    } else {
        out
    }
}

/// Find the next branch name not present in `taken`.
/// If `base` is free, returns it unchanged. Otherwise appends `-2`, `-3`, …
/// until a free name is found.
fn dedupe_branch_name(base: &str, taken: &std::collections::HashSet<String>) -> String {
    if !taken.contains(base) {
        return base.to_string();
    }
    let mut n = 2usize;
    loop {
        let candidate = format!("{}-{}", base, n);
        if !taken.contains(&candidate) {
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

    while branch.ends_with('-') {
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

    #[test]
    fn test_empty_title_with_worktree_uses_branch_name() {
        let title = resolve_title("", Some("feature-auth"), true, &[]);
        assert_eq!(title, "feature-auth");
    }

    #[test]
    fn test_empty_title_without_worktree_uses_civilization() {
        let title = resolve_title("", None, false, &[]);
        assert!(
            civilizations::CIVILIZATIONS.contains(&title.as_str()),
            "Expected a civilization name, got: {}",
            title
        );
    }

    #[test]
    fn test_provided_title_with_worktree_keeps_title() {
        let title = resolve_title("My Session", Some("feature-auth"), true, &[]);
        assert_eq!(title, "My Session");
    }

    #[test]
    fn test_provided_title_without_worktree_keeps_title() {
        let title = resolve_title("Custom Name", None, false, &[]);
        assert_eq!(title, "Custom Name");
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
    }

    #[test]
    fn test_git_sanitize_branch_name_rejects_bare_at_sign() {
        // git-check-ref-format also rejects the single character "@" as a
        // complete ref name; fall back to "session" rather than producing
        // a name libgit2 will refuse.
        assert_eq!(git_sanitize_branch_name("@"), "session");
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
        assert_eq!(
            branch_name_from_title("feat/auth.refactor"),
            "feat-auth-refactor"
        );
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
        let mut taken = std::collections::HashSet::new();
        taken.insert("fix-bug".to_string());
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-2");

        taken.insert("fix-bug-2".to_string());
        taken.insert("fix-bug-3".to_string());
        assert_eq!(dedupe_branch_name("fix-bug", &taken), "fix-bug-4");
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
            &repo_a,
            &[repo_b],
            "nonexistent-branch",
            false,
            None,
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
            &repo_a,
            &[],
            "nonexistent-branch",
            false,
            None,
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

    fn isolated_app_dir(temp_home: &std::path::Path) -> std::path::PathBuf {
        #[cfg(target_os = "linux")]
        {
            let config_home = temp_home.join(".config");
            std::env::set_var("XDG_CONFIG_HOME", &config_home);
            config_home.join(crate::session::APP_DIR_NAME_LINUX)
        }
        #[cfg(not(target_os = "linux"))]
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
}
