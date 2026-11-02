//! `agent-of-empires add` command implementation

use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::containers;
use crate::plugin::suggestions::MAX_PROMPT_BYTES;
use crate::session::builder;
use crate::session::config::repo_config;
use crate::session::{
    acquire_session_identity_lock, civilizations, duplicate_session_error, is_duplicate_session,
    GroupTree, Instance, SandboxInfo, Storage,
};

fn parse_repo_base(raw: &str) -> Result<(String, String), String> {
    let (repo, branch) = raw
        .split_once('=')
        .ok_or_else(|| format!("expected <repo>=<branch>, got '{raw}'"))?;
    if repo.trim().is_empty() || branch.trim().is_empty() {
        return Err(format!("expected <repo>=<branch>, got '{raw}'"));
    }
    Ok((repo.trim().to_string(), branch.trim().to_string()))
}

#[derive(Args)]
pub struct AddArgs {
    /// Project directory (defaults to current directory). Omit when
    /// using `--scratch`.
    path: Option<PathBuf>,

    /// Session title (defaults to folder name)
    #[arg(short = 't', long)]
    title: Option<String>,

    /// Prompt for the session name, mirroring the TUI `n` flow. Shows the
    /// generated default; press Enter to accept it. Ignored when --title
    /// is given. Requires an interactive terminal.
    #[arg(short = 'i', long)]
    interactive: bool,

    /// Group path (defaults to parent folder)
    #[arg(short = 'g', long)]
    group: Option<String>,

    /// Command to run (e.g., 'claude' or any other supported agent)
    #[arg(short = 'c', long = "cmd")]
    command: Option<String>,

    /// Named built-in or configured custom agent to run
    #[arg(long = "tool", conflicts_with = "command")]
    tool: Option<String>,

    /// Parent session (creates sub-session, inherits group, not worktree or path).
    /// Without `--worktree` or `--scratch`, opens at `<path>` (default: current
    /// directory) on its checked-out branch. `--scratch` uses a fresh directory.
    #[arg(short = 'P', long)]
    parent: Option<String>,

    /// Fork an existing session: resume its conversation context in a new,
    /// independent session that then diverges. Give the source session's id or
    /// title. Terminal fork; available for agents that support forking
    /// (claude, codex, opencode, pi).
    #[arg(long = "fork-from")]
    fork_from: Option<String>,

    /// Launch the session immediately after creating
    #[arg(short = 'l', long)]
    launch: bool,

    /// Create session in a git worktree for the specified branch
    #[arg(short = 'w', long = "worktree")]
    worktree_branch: Option<String>,

    /// Create a new branch (use with --worktree)
    #[arg(short = 'b', long = "new-branch")]
    create_branch: bool,

    /// Use a generated civilization name as the session title and as the
    /// `{branch}` value of the worktree path template, instead of the branch
    /// name (use with --worktree). The git branch stays as given. Attaching
    /// to an existing worktree keeps its directory and only generates the
    /// title. Cannot be combined with --title, --repo or --project.
    #[arg(
        long = "generated-name",
        requires = "worktree_branch",
        conflicts_with = "title"
    )]
    generated_name: bool,

    /// Branch to base the new worktree branch on (use with --new-branch).
    /// Defaults to the repository's default branch. Useful for stacking
    /// work on top of an in-flight PR branch, hot-fixing a release
    /// branch, or branching off a teammate's branch.
    #[arg(long = "base-branch")]
    base_branch: Option<String>,

    /// Base branch for one repo of a multi-repo workspace, as
    /// `<repo>=<branch>` (repeatable). `<repo>` is the repo's directory name
    /// or the path you passed to `--repo`. Outranks `--base-branch`, which
    /// stays the base for every repo this does not name. Example:
    /// `--base-branch develop --repo-base api=epic/checkout`.
    #[arg(long = "repo-base", value_parser = parse_repo_base)]
    repo_bases: Vec<(String, String)>,

    /// Additional repositories for multi-repo workspace (use with --worktree)
    #[arg(long = "repo", short = 'r')]
    extra_repos: Vec<PathBuf>,

    /// Names of registered projects to include as extra repos (use with --worktree).
    /// Resolves against the union of global + profile project registries.
    #[arg(long = "project")]
    projects: Vec<String>,

    /// Skip `git submodule update --init --recursive` after creating the
    /// worktree, overriding the `worktree.init_submodules` config (default
    /// true). Useful for repos with large or deeply nested submodule trees
    /// that you don't need inside the agent session.
    #[arg(long = "no-submodules")]
    no_submodules: bool,

    /// Run session in a container sandbox
    #[arg(short = 's', long)]
    sandbox: bool,

    /// Custom container image for sandbox (implies --sandbox)
    #[arg(long = "sandbox-image")]
    sandbox_image: Option<String>,

    /// Env var for the host `before_start` hook only, never the container
    /// (`KEY=VALUE` or `KEY`; repeatable; requires --sandbox)
    #[arg(long = "hook-env", value_name = "KEY[=VALUE]")]
    hook_env: Vec<String>,

    /// Enable YOLO mode (skip permission prompts)
    #[arg(short = 'y', long)]
    yolo: bool,

    /// Automatically trust this repository's hooks and project-local MCP
    /// servers without prompting
    #[arg(long = "trust-hooks")]
    trust_hooks: bool,

    /// Extra arguments to append after the agent binary
    #[arg(long, allow_hyphen_values = true)]
    extra_args: Option<String>,

    /// Override the agent binary command
    #[arg(long)]
    cmd_override: Option<String>,

    /// Render this session in the structured view (ACP-based native
    /// rendering) instead of the default terminal view. `aoe add` defaults
    /// to the terminal (raw tmux/PTY) so the CLI matches the TUI; pass this
    /// (or `--agent`) to opt into the structured rendering. Ignored for
    /// tools with no ACP adapter.
    #[arg(long = "structured-view")]
    structured_view: bool,

    /// First turn sent to the agent once it is ready. Structured view only;
    /// delivered when `aoe serve` starts the worker.
    #[arg(long, conflicts_with = "prompt_file")]
    prompt: Option<String>,

    /// Read the first turn from a file. Same rules as `--prompt`.
    #[arg(long = "prompt-file", value_name = "PATH", conflicts_with = "prompt")]
    prompt_file: Option<PathBuf>,

    /// Pick a specific ACP agent for the structured view (e.g., claude-code,
    /// codex).
    #[arg(long = "agent")]
    agent: Option<String>,

    /// Override the model used by the ACP agent (e.g., claude-opus-4-7,
    /// gpt-5, gemini-2.5-pro). Forwarded to the agent at session start.
    #[arg(long = "model")]
    model: Option<String>,

    /// Reasoning effort for the ACP agent (e.g., low, medium, high). Checked
    /// against the agent's known thought-level choices when cached. Structured
    /// view only.
    #[arg(long = "effort")]
    effort: Option<String>,

    /// Create the session in a fresh scratch directory under
    /// `<app_dir>/scratch/<id>/` instead of a project path. The directory is
    /// removed when the session is deleted (unless `aoe rm` is given
    /// `--keep-scratch`). Mutually exclusive with worktree-related flags.
    #[arg(
        long = "scratch",
        conflicts_with_all = [
            "worktree_branch",
            "create_branch",
            "base_branch",
            "repo_bases",
            "extra_repos",
            "projects",
            "no_submodules",
        ]
    )]
    scratch: bool,
}

const PROMPT_NEEDS_STRUCTURED: &str =
    "--prompt needs the structured view; terminal sessions cannot receive it from `aoe add`.\n\
     Pass --structured-view (or --agent) with an installed ACP adapter.";

/// The `--prompt` / `--prompt-file` text, validated before anything is created.
/// Kept as typed: only blankness is judged on the trimmed text.
fn resolve_initial_prompt(args: &AddArgs) -> Result<Option<String>> {
    let text = match (&args.prompt, &args.prompt_file) {
        (Some(text), _) => text.clone(),
        (None, Some(path)) => {
            let meta = std::fs::metadata(path)
                .with_context(|| format!("Failed to read prompt file: {}", path.display()))?;
            if !meta.is_file() {
                bail!("Prompt file is not a regular file: {}", path.display());
            }
            if meta.len() > MAX_PROMPT_BYTES as u64 {
                bail!(
                    "Prompt file is larger than {} KiB: {}",
                    MAX_PROMPT_BYTES / 1024,
                    path.display()
                );
            }
            std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read prompt file: {}", path.display()))?
        }
        (None, None) => return Ok(None),
    };
    if text.trim().is_empty() {
        bail!("The initial prompt cannot be empty");
    }
    Ok(Some(text))
}

#[tracing::instrument(target = "cli.add", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: AddArgs) -> Result<()> {
    if args.interactive && !std::io::stdin().is_terminal() {
        bail!("--interactive requires a terminal; pass --title for non-interactive naming");
    }

    crate::session::require_known_profile(profile)?;

    if args.effort.as_deref().is_some_and(|e| e.trim().is_empty()) {
        bail!("--effort cannot be empty");
    }

    let initial_prompt = resolve_initial_prompt(&args)?;

    if !args.hook_env.is_empty() && !(args.sandbox || args.sandbox_image.is_some()) {
        bail!("--hook-env requires --sandbox; host_hooks.before_start only runs for sandboxed sessions");
    }

    if args.scratch && args.path.is_some() {
        bail!(
            "Cannot specify a project path with --scratch\nTip: drop the path argument, the session runs in a fresh scratch directory"
        );
    }

    let mut path = if args.scratch {
        PathBuf::new()
    } else {
        let raw = args.path.clone().unwrap_or_else(|| PathBuf::from("."));
        if raw.as_os_str() == "." {
            std::env::current_dir()?
        } else {
            if !raw.exists() {
                bail!("Path does not exist: {}", raw.display());
            }
            raw.canonicalize()
                .with_context(|| format!("Failed to resolve path: {}", raw.display()))?
        }
    };

    if !args.scratch && !path.is_dir() {
        bail!("Path is not a directory: {}", path.display());
    }
    if !args.scratch {
        crate::session::builder::ensure_not_purging(&path)?;
    }

    if (!args.extra_repos.is_empty() || !args.projects.is_empty())
        && explicit_worktree_branch(&args).is_none()
    {
        bail!("--repo/--project requires --worktree to specify a branch\nTip: aoe add /path --project repoB -w branch-name");
    }

    if !args.repo_bases.is_empty() && explicit_worktree_branch(&args).is_none() {
        bail!("--repo-base requires --worktree to specify a branch\nTip: aoe add /path --project repoB -w branch-name --repo-base repoB=develop");
    }

    if args.generated_name && explicit_worktree_branch(&args).is_none() {
        bail!("--generated-name requires --worktree with a non-empty branch name");
    }

    if args.generated_name && (!args.extra_repos.is_empty() || !args.projects.is_empty()) {
        bail!("--generated-name does not apply to multi-repo workspaces: their directory comes from workspace_path_template, not the title");
    }

    let resolved_project_paths: Vec<PathBuf> = if args.projects.is_empty() {
        Vec::new()
    } else {
        crate::session::projects::resolve_names(profile, &args.projects)?
            .into_iter()
            .map(|p| PathBuf::from(p.path))
            .collect()
    };
    let mut all_extra_repos: Vec<PathBuf> = Vec::new();
    all_extra_repos.extend(args.extra_repos.iter().cloned());
    all_extra_repos.extend(resolved_project_paths);

    let config = if args.scratch {
        crate::session::config::profile_config::resolve_config_or_warn(profile)
    } else {
        repo_config::resolve_config_with_repo_or_warn(profile, &path)
    };

    let original_project_path = path.clone();

    let mut worktree_info_opt = None;
    let mut workspace_info_opt = None;

    let storage = Storage::new_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let worktree_session_id = uuid::Uuid::new_v4().to_string()[..8].to_string();
    let final_title =
        resolve_session_title(&args, &instances, &path, &config, &worktree_session_id)?;

    let mut resolved_tool = resolve_tool_for_add(&args, &config)?;

    let wants_structured = args.structured_view || args.agent.is_some();
    if initial_prompt.is_some() && !wants_structured {
        bail!(PROMPT_NEEDS_STRUCTURED);
    }
    if args.fork_from.is_some() && wants_structured {
        bail!(
            "`--fork-from` performs a terminal fork and cannot be combined with \
             --structured-view or --agent; structured fork is available from the web dashboard."
        );
    }

    if let Some(effort) = args.effort.as_deref().map(str::trim) {
        // The daemon records the option catalog under this key.
        let agent = args
            .agent
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&resolved_tool);
        let choices = crate::acp::option_catalog::thought_level_choices(agent);
        if let Some(warning) = check_effort_request(effort, wants_structured, agent, &choices)? {
            eprintln!("warning: {warning}");
        }
    }

    if args.fork_from.is_some() {
        if explicit_worktree_branch(&args).is_some() || args.create_branch {
            bail!(
                "`--fork-from` cannot be combined with --worktree or --new-branch: a fork must run \
                 in the parent's working directory to resume its conversation."
            );
        }
        if args.scratch {
            bail!(
                "`--fork-from` cannot be combined with --scratch: a scratch session runs in a fresh \
                 temporary directory, so the fork could not resume the parent's conversation."
            );
        }
        if args.sandbox || args.sandbox_image.is_some() {
            bail!(
                "`--fork-from` cannot be combined with --sandbox or --sandbox-image: the sandbox \
                 changes the agent's filesystem view and breaks the resumed conversation."
            );
        }
        if args.cmd_override.is_some() {
            bail!(
                "`--fork-from` cannot be combined with --cmd-override: overriding the agent binary \
                 decouples it from the parent's agent, so the fork's resume flags may not apply."
            );
        }
    }

    let fork_seed: Option<crate::session::ForkSeed> = if let Some(fork_ref) = &args.fork_from {
        let source = super::resolve_session(fork_ref, &instances)?;
        let user_chose_tool = args.tool.is_some() || args.command.is_some();
        if !user_chose_tool {
            resolved_tool = source.tool.clone();
        }
        // One rule on both surfaces: a row whose native identity cannot be
        // resolved names no conversation a fork could carry, so it is not a
        // candidate here either, exactly as the REST election drops it.
        let parent_ref = source.fork_parent_ref().unwrap_or(None);
        let seed = crate::session::fork::terminal_fork_seed(
            parent_ref,
            crate::session::capture::generate_session_uuid(),
            source.is_sandboxed(),
        )
        .map_err(|denied| {
            let profile = source.effective_profile();
            anyhow::Error::msg(denied.user_message(&source.title, &source.id, &profile))
        })?;
        Some(seed)
    } else {
        None
    };

    if let Some(branch_raw) = explicit_worktree_branch(&args) {
        use crate::git::GitWorktree;
        use crate::session::WorktreeInfo;
        use chrono::Utc;

        let branch_owned = builder::git_sanitize_branch_name(branch_raw);
        let branch = branch_owned.as_str();
        let init_submodules = config.worktree.init_submodules && !args.no_submodules;

        if !all_extra_repos.is_empty() {
            let session_base = args.base_branch.as_deref();
            let global_default = config.worktree.default_base_branch.as_deref();
            let project_bases = builder::project_base_branches(profile);
            let resolve_extra = |path: &std::path::Path| {
                let project = project_bases
                    .get(&crate::session::projects::canonical_key(
                        path.to_string_lossy(),
                    ))
                    .map(String::as_str);
                builder::resolve_base_branch(session_base, project, global_default)
            };

            let mut all_paths = vec![path.clone()];
            all_paths.extend(all_extra_repos.iter().cloned());
            let per_repo = builder::resolve_repo_base_selectors(&all_paths, &args.repo_bases)?;

            let primary = builder::WorkspaceRepoSpec {
                base_branch: per_repo
                    .get(&path)
                    .cloned()
                    .or_else(|| builder::resolve_base_branch(session_base, None, global_default)),
                path: path.clone(),
            };
            let extra_repos: Vec<builder::WorkspaceRepoSpec> = all_extra_repos
                .iter()
                .map(|p| builder::WorkspaceRepoSpec {
                    base_branch: per_repo.get(p).cloned().or_else(|| resolve_extra(p)),
                    path: p.clone(),
                })
                .collect();

            let ws_result = builder::create_workspace(
                &primary,
                &extra_repos,
                branch,
                args.create_branch,
                &config.worktree.workspace_path_template,
                init_submodules,
            )?;

            for repo in &ws_result.workspace_info.repos {
                println!(
                    "  Created worktree: {} -> {}",
                    repo.name, repo.worktree_path
                );
            }

            path = ws_result.workspace_path;
            workspace_info_opt = Some(ws_result.workspace_info);

            for w in &ws_result.warnings {
                eprintln!("⚠ {}", w);
            }

            println!("✓ Workspace created successfully");
        } else {
            if !GitWorktree::is_git_repo(&path) {
                bail!(
                    "Worktree mode requires a git repository, but this path is not one: {}\n\
                     Tip: omit --worktree-branch to start an in-place session here, \
                     or point at a git repository.",
                    path.display()
                );
            }

            let main_repo_path = GitWorktree::find_main_repo(&path)?;
            let git_wt =
                GitWorktree::new(main_repo_path.clone())?.with_init_submodules(init_submodules);

            if let Some(existing) = existing_worktree_for_branch(&git_wt, &args, branch) {
                println!(
                    "Attaching to existing worktree: {}",
                    existing.path.display()
                );
                path = existing.path;
                worktree_info_opt = Some(WorktreeInfo {
                    branch: branch.to_string(),
                    main_repo_path: main_repo_path.to_string_lossy().to_string(),
                    managed_by_aoe: false,
                    created_at: Utc::now(),
                    base_branch: None,
                });
            } else {
                let template = worktree_path_template(&config, &main_repo_path);
                let leaf_seed_owned;
                let leaf_seed = if config.session.tie_workdir_to_name || args.generated_name {
                    leaf_seed_owned =
                        crate::session::worktree_edit::worktree_leaf_from_title(&final_title);
                    leaf_seed_owned.as_str()
                } else {
                    branch
                };
                let worktree_path =
                    git_wt.compute_path(leaf_seed, template, &worktree_session_id)?;

                if worktree_path.exists() {
                    bail!(
                        "Worktree already exists at {}\nTip: Use 'aoe add {}' to add the existing worktree",
                        worktree_path.display(),
                        worktree_path.display()
                    );
                }

                println!("Creating worktree at: {}", worktree_path.display());
                let per_repo = builder::resolve_repo_base_selectors(
                    std::slice::from_ref(&main_repo_path),
                    &args.repo_bases,
                )?;
                let base = if args.create_branch {
                    per_repo.get(&main_repo_path).cloned().or_else(|| {
                        builder::resolve_base_branch(
                            args.base_branch.as_deref(),
                            None,
                            config.worktree.default_base_branch.as_deref(),
                        )
                    })
                } else {
                    None
                };
                let warnings = git_wt.create_worktree(
                    branch,
                    &worktree_path,
                    args.create_branch,
                    base.as_deref(),
                )?;

                path = worktree_path;

                worktree_info_opt = Some(WorktreeInfo {
                    branch: branch.to_string(),
                    main_repo_path: main_repo_path.to_string_lossy().to_string(),
                    managed_by_aoe: true,
                    created_at: Utc::now(),
                    base_branch: base,
                });

                for w in &warnings {
                    eprintln!("⚠ {}", w);
                }

                println!("✓ Worktree created successfully");
            }
        }
    }

    let mut group_path = args.group.clone();
    let parent_id = if let Some(parent_ref) = &args.parent {
        let parent = super::resolve_session(parent_ref, &instances)?;
        if parent.is_sub_session() {
            bail!("Cannot create sub-session of a sub-session (single level only)");
        }
        group_path = Some(parent.group_path.clone());
        Some(parent.id.clone())
    } else {
        None
    };

    if is_duplicate_session(&instances, &final_title, path.to_str().unwrap_or(""), None) {
        cleanup_partial_session(
            &path,
            worktree_info_opt.as_ref(),
            workspace_info_opt.as_ref(),
            args.create_branch,
            None,
            None,
        );
        return Err(duplicate_session_error(&final_title));
    }

    let mut instance = Instance::new(&final_title, path.to_str().unwrap_or(""));
    instance.source_profile = profile.to_string();

    if args.scratch {
        let dir = crate::session::scratch::provision_scratch_dir(&instance.id)?;
        path = dir;
        instance.project_path = path.to_string_lossy().to_string();
        instance.scratch = true;
    }

    if let Some(group) = &group_path {
        instance.group_path = group.trim().to_string();
    }

    if let Some(parent) = parent_id {
        instance.parent_session_id = Some(parent);
    }

    instance.tool = resolved_tool;
    if let Some(cmd) = &args.command {
        if cmd.trim().contains(' ') {
            instance.command = cmd.clone();
        }
    }

    instance.detect_as = config
        .session
        .agent_detect_as
        .get(&instance.tool)
        .cloned()
        .unwrap_or_default();

    if instance.command.is_empty() {
        instance.command = crate::agents::get_agent(&instance.tool)
            .filter(|a| a.set_default_command)
            .map(|a| a.binary.to_string())
            .unwrap_or_default();
    }

    if let Some(worktree_info) = worktree_info_opt {
        instance.worktree_info = Some(worktree_info);
    }

    if let Some(workspace_info) = workspace_info_opt {
        instance.workspace_info = Some(workspace_info);
    }

    crate::session::builder::apply_agent_launch_config(
        &mut instance,
        &config.session,
        args.extra_args.as_deref().unwrap_or_default(),
        args.cmd_override.as_deref().unwrap_or_default(),
        args.yolo.then_some(true),
    );

    let user_picked_agent = args.agent.is_some();
    let user_wants_structured = args.structured_view || user_picked_agent;
    instance.agent_name = args.agent.clone();
    instance.agent_model = args.model.clone();

    let registry = crate::acp::agent_registry::AgentRegistry::with_defaults();
    let agent_name = crate::acp::pick_acp_agent_name(
        &registry,
        &config.session,
        &config.acp,
        &instance.tool,
        instance.agent_name.as_deref(),
    );
    let capability_key = instance
        .agent_name
        .as_deref()
        .unwrap_or(instance.tool.as_str());
    let acp_capable = registry.get(capability_key).is_some()
        || config.session.agent_acp_cmd.contains_key(capability_key)
        || config.session.agent_acp_cmd.contains_key(&instance.tool)
        || crate::acp::inherited_acp_base(capability_key, &config.session.agent_detect_as)
            .is_some()
        || crate::acp::inherited_acp_base(&instance.tool, &config.session.agent_detect_as)
            .is_some();

    if user_picked_agent && !acp_capable {
        bail!(
            "agent `{agent_name}` is not ACP-capable: it has no registry entry and no \
                 `[session.agent_acp_cmd]` command.\n\
                 Run `aoe acp doctor` to see configured agents, or omit --agent for a \
                 terminal-view session."
        );
    }

    if args.structured_view && !acp_capable {
        bail!(
            "tool `{}` is not ACP-capable, so --structured-view has no effect.\n\
                 Run `aoe acp doctor` to see configured agents, or drop --structured-view \
                 for a terminal-view session.",
            instance.tool
        );
    }

    instance.view = if user_wants_structured && acp_capable {
        crate::session::View::Structured
    } else {
        crate::session::View::Terminal
    };

    // Set when the adapter isn't found but an on_create hook may install it, which holds only if
    // the hook still runs once trust is decided.
    let mut adapter_awaits_hooks: Option<String> = None;
    if instance.is_structured() {
        let (mut spec, spec_from_registry) = match registry.get(&agent_name) {
            Some(spec) => (spec.clone(), true),
            None => match config.session.agent_acp_cmd.get(&agent_name) {
                Some(cmd) => (
                    crate::acp::AgentSpec::from_acp_cmd(&agent_name, cmd)
                        .map_err(|e| anyhow::anyhow!(e))?,
                    false,
                ),
                None => match config.session.agent_acp_cmd.get(&instance.tool) {
                    Some(cmd) => (
                        crate::acp::AgentSpec::from_acp_cmd(&instance.tool, cmd)
                            .map_err(|e| anyhow::anyhow!(e))?,
                        false,
                    ),
                    None => match crate::acp::inherited_acp_base(
                        capability_key,
                        &config.session.agent_detect_as,
                    )
                    .or_else(|| {
                        crate::acp::inherited_acp_base(
                            &instance.tool,
                            &config.session.agent_detect_as,
                        )
                    })
                    .and_then(|base| registry.get(&base).cloned())
                    {
                        Some(spec) => (spec, true),
                        None => unreachable!("acp_capable implies a resolvable spec"),
                    },
                },
            },
        };
        if let Some(ovr) = crate::server::acp_reconciler::command_override_for_spawn(
            &instance.tool,
            &instance.command,
        ) {
            crate::acp::supervisor::apply_agent_command_override(
                &agent_name,
                spec_from_registry,
                &ovr,
                &mut spec,
            )?;
        }
        if structured_adapter_missing(
            &args,
            &config,
            &spec.command,
            std::path::Path::new(&instance.project_path),
            || containers::get_container_runtime().is_available(),
            // Trust is asked later, so without --trust-hooks the repository's hooks may still run.
            || {
                let apply = crate::session::builder::structured::preparation_hooks_apply(
                    &crate::session::config::profile_config::resolve_config_or_warn(profile).hooks,
                    &original_project_path,
                    args.scratch,
                    args.trust_hooks.then_some(true),
                );
                if apply {
                    adapter_awaits_hooks = Some(spec.command.clone());
                }
                apply
            },
        ) {
            let hint = crate::acp::install_hints::install_hint_for(&spec.command)
                .unwrap_or("install via your package manager and re-run");
            if user_picked_agent {
                cleanup_partial_session(
                    &path,
                    instance.worktree_info.as_ref(),
                    instance.workspace_info.as_ref(),
                    args.create_branch,
                    instance
                        .scratch
                        .then(|| std::path::Path::new(&instance.project_path)),
                    None,
                );
                bail!(
                    "ACP adapter `{}` is not installed or not on $PATH.\n\
                         Install: {}\n\
                         Or run: aoe acp doctor --fix\n\
                         Or use the terminal view: drop --agent / --structured-view.",
                    spec.command,
                    hint
                );
            }
            eprintln!(
                "warning: ACP adapter `{}` is not installed; this session will use the \
                     terminal view. Install it ({}) or run `aoe acp doctor --fix`, then \
                     switch the session to the structured view.",
                spec.command, hint
            );
            instance.view = crate::session::View::Terminal;
        }
    }

    let explicit_effort = args
        .effort
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);

    if explicit_effort.is_some() && !instance.is_structured() {
        cleanup_partial_session(
            &path,
            instance.worktree_info.as_ref(),
            instance.workspace_info.as_ref(),
            args.create_branch,
            if instance.scratch {
                Some(std::path::Path::new(&instance.project_path))
            } else {
                None
            },
            None,
        );
        bail!(
            "--effort needs the structured view, but the ACP adapter is not installed.\n\
             Run `aoe acp doctor --fix`, then re-run."
        );
    }

    if initial_prompt.is_some() && !instance.is_structured() {
        cleanup_partial_session(
            &path,
            instance.worktree_info.as_ref(),
            instance.workspace_info.as_ref(),
            args.create_branch,
            if instance.scratch {
                Some(std::path::Path::new(&instance.project_path))
            } else {
                None
            },
            None,
        );
        bail!(PROMPT_NEEDS_STRUCTURED);
    }
    instance.pending_initial_turn = initial_prompt.map(|text| crate::session::PendingInitialTurn {
        text,
        attachments: Vec::new(),
        synthesized: false,
        error: None,
    });

    if instance.is_structured() {
        let defaults = config.acp.acp_defaults_for(&agent_name);
        instance.agent_model = crate::session::config::resolve_spawn_model_effort(
            defaults,
            instance.agent_model.take(),
            explicit_effort.clone(),
        )
        .0;
        instance.acp_effort = explicit_effort;
    }

    let use_sandbox = args.sandbox || args.sandbox_image.is_some();

    let runtime = containers::get_container_runtime();
    if use_sandbox || config.sandbox.enabled_by_default {
        if !runtime.is_available() {
            if use_sandbox {
                bail!(
                    "Container runtime is not installed or not accessible.\n\
                     Install a supported runtime to use sandbox mode.\n\
                     Tip: Use 'aoe add' without --sandbox to run directly on host"
                );
            }
        } else {
            for w in sandbox_env_warnings(&config.sandbox.environment, &args.hook_env) {
                eprintln!("⚠ {}", w);
            }

            let container_name = containers::DockerContainer::generate_name(&instance.id);
            let image = resolve_sandbox_image(
                args.sandbox_image.as_deref(),
                &config.sandbox.default_image,
                runtime.default_sandbox_image(),
            );
            instance.sandbox_info = Some(SandboxInfo {
                enabled: true,
                container_id: None,
                image,
                container_name,
                extra_env: None,
                custom_instruction: config.sandbox.custom_instruction.clone(),
                before_start_env: Vec::new(),
                before_start_hook_env: args.hook_env.clone(),
                provider: None,
                container_workdir: None,
            });
        }
    }

    if let Some(seed) = fork_seed {
        match seed {
            crate::session::ForkSeed::Terminal {
                parent,
                child_session_id,
                unattributed_parent_agent,
            } => crate::session::fork::apply_terminal_fork_seed(
                &mut instance,
                &config.session,
                profile,
                *parent,
                child_session_id,
                unattributed_parent_agent.as_deref(),
            )?,
            crate::session::ForkSeed::Structured { .. } => {}
        }
    }

    let hook_result: Result<()> = (|| {
        let resolved_hooks: Option<repo_config::ResolvedHooks> = if args.scratch {
            repo_config::ResolvedHooks::global(profile)
        } else {
            use repo_config::TrustSurface;
            match repo_config::check_repo_trust(&original_project_path) {
                Ok(trust) => {
                    let repo_root = std::path::Path::new(&trust.project_path);
                    let repo_hooks: Option<crate::session::HooksConfig> = match &trust.hooks {
                        TrustSurface::Trusted(h) => Some(h.clone()),
                        TrustSurface::NeedsTrust { config, .. } => Some(config.clone()),
                        TrustSurface::Absent => None,
                    };
                    let hooks_hash_write = match &trust.hooks {
                        TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
                        _ => None,
                    };
                    let mcp_hash_write = match &trust.mcp {
                        TrustSurface::NeedsTrust { hash, .. } => Some(hash.clone()),
                        _ => None,
                    };
                    let mcp_servers = match &trust.mcp {
                        TrustSurface::Trusted(s) | TrustSurface::NeedsTrust { config: s, .. } => {
                            Some(s.clone())
                        }
                        TrustSurface::Absent => None,
                    };

                    let approved = if !trust.needs_prompt() || args.trust_hooks {
                        true
                    } else {
                        if let Some(ref hooks) = repo_hooks {
                            println!(
                                "\nHooks for this session (repo overrides global config per type):"
                            );
                            let merged = repo_config::merge_hooks_for_display(profile, hooks);
                            for group in repo_config::hook_display_groups(&merged, hooks, true) {
                                println!("  {}:{}", group.name, group.source_label());
                                for cmd in &group.commands {
                                    println!("    {}", cmd);
                                }
                            }
                        }
                        if let Some(ref servers) = mcp_servers {
                            println!("\nProject MCP servers from .mcp.json (values redacted):");
                            for server in servers {
                                println!("  {}", server.redacted_summary());
                            }
                        }
                        print!("\nTrust this repo (hooks and project MCP shown above)? [y/N] ");
                        use std::io::Write;
                        std::io::stdout().flush()?;
                        let mut input = String::new();
                        std::io::stdin().read_line(&mut input)?;
                        input.trim().eq_ignore_ascii_case("y")
                    };

                    if approved {
                        if hooks_hash_write.is_some() || mcp_hash_write.is_some() {
                            repo_config::trust_repo(
                                &original_project_path,
                                hooks_hash_write.as_deref(),
                                mcp_hash_write.as_deref(),
                            )?;
                            if hooks_hash_write.is_some() {
                                println!("✓ Repository hooks trusted");
                            }
                            if mcp_hash_write.is_some() {
                                println!("✓ Project MCP servers trusted");
                            }
                        }
                        match repo_hooks {
                            Some(h) => repo_config::ResolvedHooks::with_repo(profile, repo_root, h),
                            None => repo_config::ResolvedHooks::global(profile),
                        }
                    } else {
                        println!(
                            "Skipped (session created without trusting repo hooks or project MCP)"
                        );
                        hooks_when_trust_declined(profile, repo_root, &trust.hooks)
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "cli.add", "Failed to check repo trust: {}", e);
                    repo_config::ResolvedHooks::global(profile)
                }
            }
        };

        refuse_adapter_without_hooks(adapter_awaits_hooks.as_deref(), resolved_hooks.as_ref())?;

        if let Some(resolved) = resolved_hooks {
            let commands = &resolved.hooks().on_create;
            if !commands.is_empty() {
                println!("Running on_create hooks:");
                for cmd in commands {
                    println!("  {}", cmd);
                }
                let hook_env = repo_config::lifecycle_env_vars(&instance);
                if instance.sandbox_info.is_some() {
                    instance.get_container_for_instance()?;
                }
                let ran = match instance.sandbox_info {
                    Some(ref sandbox) => repo_config::execute_hooks_in_container(
                        commands,
                        &sandbox.container_name,
                        &instance.container_workdir(),
                        &hook_env,
                    ),
                    None => repo_config::execute_hooks(commands, &path, &hook_env),
                };
                if let Err(e) = ran {
                    let hint = resolved
                        .origin_hint("on_create")
                        .map(|hint| format!("\n{hint}"))
                        .unwrap_or_default();
                    anyhow::bail!("on_create hook failed: {e:#}{hint}");
                }
                println!("✓ on_create hooks completed");
            }
        }
        Ok(())
    })();

    if let Err(e) = hook_result {
        cleanup_partial_session(
            &path,
            instance.worktree_info.as_ref(),
            instance.workspace_info.as_ref(),
            args.create_branch,
            if instance.scratch {
                Some(std::path::Path::new(&instance.project_path))
            } else {
                None
            },
            instance.sandbox_info.as_ref().map(|_| instance.id.as_str()),
        );
        return Err(e);
    }

    let _identity_lock = match acquire_session_identity_lock() {
        Ok(lock) => lock,
        Err(error) => {
            cleanup_partial_session(
                &path,
                instance.worktree_info.as_ref(),
                instance.workspace_info.as_ref(),
                args.create_branch,
                if instance.scratch {
                    Some(std::path::Path::new(&instance.project_path))
                } else {
                    None
                },
                instance.sandbox_info.as_ref().map(|_| instance.id.as_str()),
            );
            return Err(error);
        }
    };

    let persist_result = storage.update(|all_instances, groups| {
        if is_duplicate_session(
            all_instances.iter(),
            &instance.title,
            instance.project_path.as_str(),
            None,
        ) {
            return Ok(false);
        }
        all_instances.push(instance.clone());
        if !instance.group_path.is_empty() {
            let mut group_tree = GroupTree::new_with_groups(all_instances, groups);
            group_tree.create_group(&instance.group_path);
            *groups = group_tree.get_all_groups();
        }
        Ok(true)
    });
    match persist_result {
        Ok(true) => {
            crate::tips::record_session_creations(1);
            tracing::info!(
                target: "session.create",
                session = %instance.id,
                title = %instance.title,
                tool = %instance.tool,
                view = if instance.is_structured() { "structured" } else { "terminal" },
                project_path = %instance.project_path,
                branch = ?instance.worktree_info.as_ref().map(|w| &w.branch),
                "session created"
            );
        }
        Ok(false) => {
            cleanup_partial_session(
                &path,
                instance.worktree_info.as_ref(),
                instance.workspace_info.as_ref(),
                args.create_branch,
                if instance.scratch {
                    Some(std::path::Path::new(&instance.project_path))
                } else {
                    None
                },
                instance.sandbox_info.as_ref().map(|_| instance.id.as_str()),
            );
            return Err(duplicate_session_error(&instance.title));
        }
        Err(e) => {
            cleanup_partial_session(
                &path,
                instance.worktree_info.as_ref(),
                instance.workspace_info.as_ref(),
                args.create_branch,
                if instance.scratch {
                    Some(std::path::Path::new(&instance.project_path))
                } else {
                    None
                },
                instance.sandbox_info.as_ref().map(|_| instance.id.as_str()),
            );
            return Err(e);
        }
    }
    drop(_identity_lock);

    println!("✓ Added session: {}", final_title);
    println!("  Profile: {}", storage.profile());
    println!("  Path:    {}", path.display());
    println!("  Group:   {}", instance.group_path);
    println!("  ID:      {}", instance.id);
    if let Some(cmd) = &args.command {
        println!("  Cmd:     {}", cmd);
    }
    if let Some(parent) = &args.parent {
        println!("  Parent:  {}", parent);
    }
    if instance.sandbox_info.is_some() {
        println!("  Sandbox: enabled");
    }
    if instance.scratch {
        println!("  Scratch:  yes");
    }
    if instance.yolo_mode {
        println!("  YOLO:    enabled");
    }
    if let Some(ws) = &instance.workspace_info {
        println!("  Workspace: {} repos", ws.repos.len());
        for repo in &ws.repos {
            println!("    - {} ({})", repo.name, repo.worktree_path);
        }
    }

    let is_acp = instance.is_structured();

    if is_acp {
        println!();
        println!("Next steps:");
        println!("  aoe serve                   # Start the dashboard (worker auto-spawns)");
        println!("  Open the printed URL and select '{}'.", final_title);
        if args.launch {
            println!();
            println!(
                "(--launch is a no-op for structured view sessions; \
                 lifecycle is managed by `aoe serve`.)"
            );
        }
    } else if args.launch {
        let id = instance.id.clone();
        match instance.start_with_size(crate::terminal::get_size()) {
            Ok(()) => {
                let landed = storage.update(|all_instances, _groups| {
                    if let Some(stored) = all_instances.iter_mut().find(|i| i.id == id) {
                        stored.merge_post_start(&instance);
                        Ok(true)
                    } else {
                        tracing::warn!(
                            target: "session.cli",
                            session_id = %id,
                            "session row removed by peer between insert and launch-merge; tmux session is now orphan"
                        );
                        Ok(false)
                    }
                })?;
                if !landed {
                    anyhow::bail!(
                        "Session {} was removed by another process before launch could land; tmux session is now orphan",
                        instance.title
                    );
                }

                let tmux_session = crate::tmux::Session::new(&instance.id, &instance.title)?;
                if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
                    tmux_session.attach()?;
                } else {
                    println!(
                        "(no controlling terminal; session started without attaching. \
                         Use `aoe -p {} session attach {}` to view it.)",
                        shell_words::quote(storage.profile()),
                        shell_words::quote(&instance.id)
                    );
                }

                let file_watch = crate::file_watch::FileWatchService::noop();
                crate::session::sync::capture_launched_session_id_blocking(
                    &mut instance,
                    &file_watch,
                    crate::session::sync::CLI_ATTACHED_SESSION_ID_CAPTURE_TIMEOUT,
                    true,
                );
            }
            Err(e) => {
                if let Err(rollback_err) = storage.update(|all_instances, _groups| {
                    if let Some(stored) = all_instances.iter_mut().find(|i| i.id == id) {
                        stored.status = crate::session::Status::Error;
                    }
                    Ok(())
                }) {
                    tracing::error!(
                        target: "session.store",
                        "Failed to persist Status::Error rollback for {}: {}; row may show stale Starting status",
                        id,
                        rollback_err
                    );
                }
                eprintln!(
                    "Warning: launch failed: {}. Retry with: aoe session start {}",
                    e, final_title
                );
                return Err(e);
            }
        }
    } else {
        println!();
        println!("Next steps:");
        println!(
            "  aoe session start {}   # Start the session",
            shell_words::quote(&final_title)
        );
        println!("  aoe                         # Open TUI and press Enter to attach");
    }

    Ok(())
}

fn resolve_session_title(
    args: &AddArgs,
    instances: &[Instance],
    path: &std::path::Path,
    config: &crate::session::Config,
    worktree_session_id: &str,
) -> Result<String> {
    if let Some(title) = &args.title {
        return Ok(title.trim().to_string());
    }
    let existing_titles: Vec<&str> = instances.iter().map(|i| i.title.as_str()).collect();
    let default_title = if args.generated_name {
        generated_worktree_title(args, path, config, &existing_titles, worktree_session_id)?
    } else if let Some(branch) = explicit_worktree_branch(args) {
        branch.to_string()
    } else {
        civilizations::generate_random_title(&existing_titles)
    };
    if args.interactive {
        prompt_session_title(&default_title)
    } else {
        Ok(default_title)
    }
}

/// The worktree `--worktree` attaches to instead of creating one (`-b` always creates).
fn existing_worktree_for_branch(
    git_wt: &crate::git::GitWorktree,
    args: &AddArgs,
    branch: &str,
) -> Option<crate::git::WorktreeEntry> {
    if args.create_branch {
        return None;
    }
    git_wt
        .list_worktrees()
        .ok()?
        .into_iter()
        .find(|wt| wt.branch.as_deref() == Some(branch))
}

/// A civilization title unused by any session. When a new worktree will be created, its
/// directory (computed with the session id creation will use) must also be free; attaching,
/// or a path that is not a git repo, skips that check (creation reports the latter).
fn generated_worktree_title(
    args: &AddArgs,
    path: &std::path::Path,
    config: &crate::session::Config,
    existing_titles: &[&str],
    worktree_session_id: &str,
) -> Result<String> {
    use crate::git::GitWorktree;

    let git_wt = GitWorktree::find_main_repo(path)
        .and_then(GitWorktree::new)
        .ok();
    let branch = explicit_worktree_branch(args).map(builder::git_sanitize_branch_name);
    let attaching = match (&git_wt, &branch) {
        (Some(git_wt), Some(branch)) => {
            existing_worktree_for_branch(git_wt, args, branch).is_some()
        }
        _ => false,
    };
    let dir_taken = |candidate: &str| {
        let Some(git_wt) = git_wt.as_ref().filter(|_| !attaching) else {
            return false;
        };
        let template = worktree_path_template(config, &git_wt.repo_path);
        let leaf = crate::session::worktree_edit::worktree_leaf_from_title(candidate);
        git_wt
            .compute_path(&leaf, template, worktree_session_id)
            .is_ok_and(|p| p.exists())
    };
    civilizations::generate_random_title_filtered(existing_titles, dir_taken).context(
        "Could not generate a unique worktree name; drop --generated-name and pass --title",
    )
}

fn worktree_path_template<'a>(
    config: &'a crate::session::Config,
    main_repo_path: &std::path::Path,
) -> &'a str {
    if crate::git::GitWorktree::is_bare_repo(main_repo_path) {
        &config.worktree.bare_repo_path_template
    } else {
        &config.worktree.path_template
    }
}

fn explicit_worktree_branch(args: &AddArgs) -> Option<&str> {
    args.worktree_branch
        .as_deref()
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
}

fn prompt_session_title(default_title: &str) -> Result<String> {
    use std::io::Write;

    eprint!("Session name [{}]: ", default_title);
    std::io::stderr().flush()?;

    let mut input = String::new();
    let read = std::io::stdin().read_line(&mut input)?;
    if read == 0 {
        return Ok(default_title.to_string());
    }

    let trimmed = input.trim();
    Ok(if trimmed.is_empty() {
        default_title.to_string()
    } else {
        trimmed.to_string()
    })
}

/// Refuse `--effort` on a session that will not be structured, then check it
/// against the agent's choices.
fn check_effort_request(
    effort: &str,
    wants_structured: bool,
    agent: &str,
    choices: &[String],
) -> Result<Option<String>> {
    if !wants_structured {
        bail!("--effort needs the structured view; pass --agent or --structured-view");
    }
    check_effort(agent, effort, choices)
}

/// Fail when the agent's thought-level choices are known and `effort` is not
/// one of them; warn when they are not known yet.
fn check_effort(agent: &str, effort: &str, choices: &[String]) -> Result<Option<String>> {
    if choices.is_empty() {
        return Ok(Some(format!(
            "no known effort choices for ACP agent `{agent}`; passing `{effort}` through unchecked"
        )));
    }
    if choices.iter().any(|choice| choice == effort) {
        return Ok(None);
    }
    bail!(
        "effort `{effort}` is not offered by ACP agent `{agent}`. Valid values: {}",
        choices.join(", ")
    );
}

fn cleanup_partial_session(
    path: &std::path::Path,
    worktree_info: Option<&crate::session::WorktreeInfo>,
    workspace_info: Option<&crate::session::WorkspaceInfo>,
    created_branch: bool,
    scratch_dir: Option<&std::path::Path>,
    container_session_id: Option<&str>,
) {
    if let Some(session_id) = container_session_id {
        let container = crate::containers::DockerContainer::from_session_id(session_id);
        if let crate::containers::Teardown::Failed(e) = container.teardown(session_id) {
            tracing::warn!(
                target: "cli.add",
                "failed to remove sandbox container during partial cleanup for {}: {}",
                session_id,
                e
            );
        }
    }
    if let Some(wt) = worktree_info {
        if wt.managed_by_aoe {
            if let Ok(git_wt) = crate::git::GitWorktree::new(PathBuf::from(&wt.main_repo_path)) {
                let _ = git_wt.remove_worktree(path, false);
                if created_branch {
                    let _ = git_wt.delete_branch(&wt.branch);
                }
            }
        }
    }
    if let Some(ws) = workspace_info {
        for repo in &ws.repos {
            if repo.managed_by_aoe {
                if let Ok(git_wt) =
                    crate::git::GitWorktree::new(PathBuf::from(&repo.main_repo_path))
                {
                    let _ =
                        git_wt.remove_worktree(std::path::Path::new(&repo.worktree_path), false);
                    if created_branch && !repo.branch_preexisting {
                        let _ = git_wt.delete_branch(&repo.branch);
                    }
                }
            }
        }
        let _ = std::fs::remove_dir_all(&ws.workspace_dir);
    }
    if let Some(scratch) = scratch_dir {
        if crate::session::scratch::is_scratch_path(scratch) {
            let _ = std::fs::remove_dir_all(scratch);
        }
    }
}

/// Once trust is decided, refuse the adapter the first check let through for a preparation
/// hook if declining trust left no such hook to run.
fn refuse_adapter_without_hooks(
    adapter_awaits_hooks: Option<&str>,
    resolved: Option<&repo_config::ResolvedHooks>,
) -> Result<()> {
    let Some(command) = adapter_awaits_hooks else {
        return Ok(());
    };
    if resolved.is_some_and(|resolved| {
        let hooks = resolved.hooks();
        !hooks.on_create.is_empty() || !hooks.on_launch.is_empty()
    }) {
        return Ok(());
    }
    let hint = crate::acp::install_hints::install_hint_for(command)
        .unwrap_or("install via your package manager and re-run");
    bail!(
        "ACP adapter `{command}` is not installed or not on $PATH, and no on_create or \
         on_launch hook will run to install it (the repository's hooks weren't trusted).\n\
         Install: {hint}\n\
         Or re-run with --trust-hooks, or use the terminal view: drop --agent / --structured-view."
    )
}

/// Whether a structured `aoe add` must refuse, or fall back, for its adapter. A sandboxed
/// adapter runs inside the container, which bundles it, so a sandbox the runtime can start
/// skips the host check. `preparation_hooks` is asked only when the adapter isn't found:
/// an `on_create` or `on_launch` hook that runs first may install it.
fn structured_adapter_missing(
    args: &AddArgs,
    config: &crate::session::Config,
    command: &str,
    project_path: &std::path::Path,
    runtime_available: impl FnOnce() -> bool,
    preparation_hooks: impl FnOnce() -> bool,
) -> bool {
    let sandboxed =
        (args.sandbox || args.sandbox_image.is_some() || config.sandbox.enabled_by_default)
            && runtime_available();
    !sandboxed
        && !crate::session::builder::structured::adapter_reachable(
            command,
            Some(project_path),
            config,
        )
        && !preparation_hooks()
}

fn resolve_tool_for_add(args: &AddArgs, config: &crate::session::Config) -> Result<String> {
    let tool_name = if let Some(tool) = &args.tool {
        let selection = resolve_named_tool(tool, config)?;
        if selection.is_custom() && args.cmd_override.is_some() {
            bail!("--cmd-override cannot be used with configured custom agent --tool selections");
        }
        selection.name().to_string()
    } else if let Some(cmd) = &args.command {
        let tool_name = detect_tool(cmd)?;
        match override_launch_binary(&tool_name, &config.session) {
            Some(bin) => {
                if !crate::tmux::is_binary_on_path(&bin) {
                    bail!(
                        "'{}' (from session.agent_command_override) is not installed or not on $PATH.\n\
                         See all supported agents: aoe agents",
                        bin
                    );
                }
            }
            None => {
                if let Some(agent_def) = crate::agents::get_agent(&tool_name) {
                    if !crate::tmux::is_agent_available(agent_def) {
                        bail!(
                            "'{}' is not installed or not on $PATH.\n\
                             Install with: {}\n\
                             See all supported agents: aoe agents",
                            agent_def.binary,
                            agent_def.install_hint
                        );
                    }
                }
            }
        }
        tool_name
    } else {
        let available_tools = crate::tmux::AvailableTools::detect();
        let tools_list = available_tools.available_list();
        config
            .session
            .default_tool
            .as_deref()
            .and_then(|name| {
                if config.session.custom_agents.contains_key(name) {
                    Some(name)
                } else {
                    crate::agents::resolve_tool_name(name)
                }
            })
            .or_else(|| tools_list.first().map(|s| s.as_str()))
            .unwrap_or("claude")
            .to_string()
    };

    if let Some(notice) =
        crate::agents::get_agent(&tool_name).and_then(crate::agents::AgentDef::lifecycle_notice)
    {
        eprintln!("Warning: {tool_name} is {notice}");
    }
    Ok(tool_name)
}

fn detect_tool(cmd: &str) -> Result<String> {
    crate::agents::resolve_tool_name(cmd)
        .map(|name| name.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Unknown tool in command: {}\n\
                 Supported tools: {}\n\
                 Tip: Command must contain one of the supported tool names",
                cmd,
                crate::agents::agent_names().join(", ")
            )
        })
}

/// Declined trust keeps already-trusted repo hooks; unapproved ones fall back
/// to the personal global and profile hooks.
fn hooks_when_trust_declined(
    profile: &str,
    repo_root: &std::path::Path,
    hooks: &repo_config::TrustSurface<crate::session::HooksConfig>,
) -> Option<repo_config::ResolvedHooks> {
    match hooks {
        repo_config::TrustSurface::Trusted(h) => {
            repo_config::ResolvedHooks::with_repo(profile, repo_root, h.clone())
        }
        repo_config::TrustSurface::NeedsTrust { .. } | repo_config::TrustSurface::Absent => {
            repo_config::ResolvedHooks::global(profile)
        }
    }
}

fn override_launch_binary(
    tool: &str,
    session: &crate::session::config::SessionConfig,
) -> Option<String> {
    let command = session.resolve_tool_command(tool);
    shell_words::split(&command).ok()?.into_iter().next()
}

enum NamedToolSelection {
    Custom(String),
    BuiltIn(String),
}

impl NamedToolSelection {
    fn name(&self) -> &str {
        match self {
            Self::Custom(name) | Self::BuiltIn(name) => name,
        }
    }

    fn is_custom(&self) -> bool {
        matches!(self, Self::Custom(_))
    }
}

fn resolve_named_tool(tool: &str, config: &crate::session::Config) -> Result<NamedToolSelection> {
    let name = tool.trim();
    if name.is_empty() {
        bail!("--tool requires a non-empty agent name");
    }

    if let Some(command) = config.session.custom_agents.get(name) {
        if command.trim().is_empty() {
            bail!("custom agent '{name}' has an empty configured command");
        }
        if let Some(detect_as) = config
            .session
            .agent_detect_as
            .get(name)
            .map(|target| target.trim())
            .filter(|target| !target.is_empty())
        {
            if crate::agents::get_agent(detect_as).is_none() {
                bail!(
                    "custom agent '{name}' maps agent_detect_as to unknown agent '{detect_as}'. Known agents: {}",
                    crate::agents::agent_names().join(", ")
                );
            }
        }
        return Ok(NamedToolSelection::Custom(name.to_string()));
    }

    if let Some(tool_name) = crate::agents::resolve_tool_name(name) {
        if let Some(agent_def) = crate::agents::get_agent(tool_name) {
            if !crate::tmux::is_agent_available(agent_def) {
                bail!(
                    "'{}' is not installed or not on $PATH.\n\
                     Install with: {}\n\
                     See all supported agents: aoe agents",
                    agent_def.binary,
                    agent_def.install_hint
                );
            }
        }
        return Ok(NamedToolSelection::BuiltIn(tool_name.to_string()));
    }

    let mut safe_names: Vec<String> = crate::agents::agent_names()
        .into_iter()
        .map(str::to_string)
        .collect();
    safe_names.extend(
        config
            .session
            .custom_agents
            .keys()
            .filter(|name| !name.is_empty())
            .cloned(),
    );
    safe_names.sort();
    safe_names.dedup();

    bail!(
        "Unknown tool: {name}\nSupported built-in and configured custom agents: {}",
        safe_names.join(", ")
    )
}

fn sandbox_env_warnings(sandbox_env: &[String], hook_env: &[String]) -> Vec<String> {
    crate::session::validate_env_entries(sandbox_env.iter().chain(hook_env))
}

fn resolve_sandbox_image(
    flag: Option<&str>,
    merged_default: &str,
    hardcoded_default: &str,
) -> String {
    if let Some(flag) = flag {
        return flag.trim().to_string();
    }
    let merged = merged_default.trim();
    if merged.is_empty() {
        hardcoded_default.to_string()
    } else {
        merged.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{
        check_effort_request, hooks_when_trust_declined, override_launch_binary, parse_repo_base,
        resolve_sandbox_image,
    };
    use crate::session::config::repo_config::TrustSurface;
    use crate::session::config::SessionConfig;
    use crate::session::HooksConfig;

    #[test]
    fn declined_trust_keeps_personal_on_create_hooks_only() {
        let _app = crate::session::test_support::isolate_app_dir();
        let write = |path: std::path::PathBuf, body: &str| {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(
            crate::session::get_app_dir().unwrap().join("config.toml"),
            "[hooks]\non_create = [\"global-create\"]\n",
        );
        write(
            crate::session::get_profile_dir_path("work")
                .unwrap()
                .join("config.toml"),
            "[hooks]\non_create = [\"profile-create\"]\n",
        );
        let repo = HooksConfig {
            on_create: vec!["repo-create".into()],
            on_launch: vec!["repo-launch".into()],
            ..Default::default()
        };
        let root = std::path::Path::new("/repo");

        for (profile, expected) in [("default", "global-create"), ("work", "profile-create")] {
            let unapproved = TrustSurface::NeedsTrust {
                config: repo.clone(),
                hash: "h".into(),
            };
            for surface in [unapproved, TrustSurface::Absent] {
                let hooks = hooks_when_trust_declined(profile, root, &surface)
                    .expect("personal on_create hooks must survive a declined prompt");
                assert_eq!(
                    hooks.hooks().on_create,
                    vec![expected.to_string()],
                    "{profile}"
                );
                assert!(hooks.hooks().on_launch.is_empty(), "{profile}");
            }
            let trusted =
                hooks_when_trust_declined(profile, root, &TrustSurface::Trusted(repo.clone()))
                    .unwrap();
            assert_eq!(trusted.hooks().on_create, vec!["repo-create".to_string()]);
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn hook_env_requires_sandbox() {
        use clap::Parser;
        let _app = crate::session::test_support::isolate_app_dir();
        let parse = |extra: &[&str]| {
            let mut argv = vec!["aoe", "add", "--hook-env", "MINT_REPO=a/b"];
            argv.extend_from_slice(extra);
            match crate::cli::Cli::try_parse_from(argv).unwrap().command {
                Some(crate::cli::Commands::Add(args)) => *args,
                _ => panic!("expected an add invocation"),
            }
        };
        let err = super::run("default", parse(&[])).await.unwrap_err();
        assert!(err.to_string().contains("--hook-env requires --sandbox"));
        assert_eq!(parse(&["-s"]).hook_env, ["MINT_REPO=a/b"]);
    }

    fn parse_add(argv: &[&str]) -> Result<super::AddArgs, clap::Error> {
        use clap::Parser;
        match crate::cli::Cli::try_parse_from(argv)?.command {
            Some(crate::cli::Commands::Add(args)) => Ok(*args),
            _ => panic!("expected an add invocation"),
        }
    }

    #[test]
    fn initial_prompt_flags_are_exclusive_and_validated_as_typed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("p.md");
        let file_arg = file.to_str().unwrap();
        assert!(parse_add(&["aoe", "add", "--prompt", "a", "--prompt-file", file_arg]).is_err());

        std::fs::write(&file, "  line one\nline two\n").unwrap();
        for (argv, expected) in [
            (vec!["aoe", "add"], Ok(None)),
            (vec!["aoe", "add", "--prompt", "  go "], Ok(Some("  go "))),
            (
                vec!["aoe", "add", "--prompt-file", file_arg],
                Ok(Some("  line one\nline two\n")),
            ),
            (
                vec!["aoe", "add", "--prompt", "  \n"],
                Err("cannot be empty"),
            ),
            (
                vec!["aoe", "add", "--prompt-file", "/nonexistent/aoe-prompt"],
                Err("Failed to read prompt file"),
            ),
            (
                vec!["aoe", "add", "--prompt-file", dir.path().to_str().unwrap()],
                Err("not a regular file"),
            ),
        ] {
            let got = super::resolve_initial_prompt(&parse_add(&argv).unwrap());
            match expected {
                Ok(text) => assert_eq!(got.unwrap().as_deref(), text, "{argv:?}"),
                Err(msg) => assert!(got.unwrap_err().to_string().contains(msg), "{argv:?}"),
            }
        }
    }

    #[test]
    fn an_oversized_prompt_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.md");
        std::fs::write(&file, "x".repeat(super::MAX_PROMPT_BYTES + 1)).unwrap();
        let args = parse_add(&["aoe", "add", "--prompt-file", file.to_str().unwrap()]).unwrap();
        let err = super::resolve_initial_prompt(&args).unwrap_err();
        assert!(err.to_string().contains("larger than 64 KiB"), "{err}");
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_structured_add_stores_the_prompt_verbatim_as_the_pending_turn() {
        let root = tempfile::tempdir().unwrap();
        let _app = crate::session::test_support::isolate_app_dir_at(root.path());
        // `--tool claude` is checked against the process PATH, which the adapter
        // stub's configured PATH does not touch.
        let _claude = crate::session::test_support::install_login_shell_path_command(
            root.path(),
            "claude",
            "#!/bin/sh\nexit 1\n",
        );
        let _adapter = crate::session::test_support::stub_claude_adapter_on_agent_path();
        let work = tempfile::tempdir().unwrap();
        let file = work.path().join("prompt.md");
        std::fs::write(&file, "  indented first\nsecond\n").unwrap();
        for (title, flag, value, expected) in [
            ("inline", "--prompt", "  go now ", "  go now "),
            (
                "from-file",
                "--prompt-file",
                file.to_str().unwrap(),
                "  indented first\nsecond\n",
            ),
        ] {
            let project = tempfile::tempdir().unwrap();
            let args = parse_add(&[
                "aoe",
                "add",
                project.path().to_str().unwrap(),
                "-t",
                title,
                "--tool",
                "claude",
                "--structured-view",
                flag,
                value,
            ])
            .unwrap();
            super::run("default", args).await.unwrap();

            let (rows, _) = crate::session::Storage::new_unwatched("default")
                .unwrap()
                .load_with_groups()
                .unwrap();
            let turn = rows
                .iter()
                .find(|i| i.title == title)
                .and_then(|i| i.pending_initial_turn.clone())
                .unwrap_or_else(|| panic!("{title}: no pending turn stored"));
            assert_eq!(turn.text, expected, "{title}");
            assert!(turn.attachments.is_empty() && !turn.synthesized && turn.error.is_none());
        }
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn a_prompt_is_refused_for_a_terminal_session_and_nothing_is_stored() {
        let _app = crate::session::test_support::isolate_app_dir();
        let project = tempfile::tempdir().unwrap();
        let err = super::run(
            "default",
            parse_add(&[
                "aoe",
                "add",
                project.path().to_str().unwrap(),
                "--prompt",
                "go",
            ])
            .unwrap(),
        )
        .await
        .unwrap_err();
        assert!(err
            .to_string()
            .contains("--prompt needs the structured view"));
        let (rows, _) = crate::session::Storage::new_unwatched("default")
            .unwrap()
            .load_with_groups()
            .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn sandbox_env_warnings_cover_hook_env() {
        let unset = "AOE_TEST_HOOK_ENV_UNSET_9f3a";
        let hook = vec![format!("GH_TOKEN=${unset}")];
        let warnings = super::sandbox_env_warnings(&[], &hook);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains(unset));
        assert!(super::sandbox_env_warnings(&[], &[]).is_empty());
    }

    #[test]
    fn parse_repo_base_splits_on_the_first_equals() {
        let ok = [
            ("api=develop", ("api", "develop")),
            ("/src/api=epic/a=b", ("/src/api", "epic/a=b")),
            (" api = develop ", ("api", "develop")),
        ];
        for (raw, (repo, branch)) in ok {
            assert_eq!(
                parse_repo_base(raw).unwrap(),
                (repo.to_string(), branch.to_string()),
                "{raw:?}"
            );
        }
        for raw in ["api", "=develop", "api=", "  =  "] {
            assert!(parse_repo_base(raw).is_err(), "{raw:?} should be rejected");
        }
    }

    const HARDCODED: &str = "ghcr.io/agent-of-empires/aoe-sandbox:latest";

    #[test]
    fn override_launch_binary_takes_the_override_program() {
        for (override_cmd, expected) in [
            (None, None),
            (Some("opencode-plannotator"), Some("opencode-plannotator")),
            (Some("ocp run sp"), Some("ocp")),
            (
                Some("\"/opt/My Wrapper/opencode\" --mode plan"),
                Some("/opt/My Wrapper/opencode"),
            ),
        ] {
            let mut session = SessionConfig::default();
            if let Some(cmd) = override_cmd {
                session
                    .agent_command_override
                    .insert("opencode".to_string(), cmd.to_string());
            }
            assert_eq!(
                override_launch_binary("opencode", &session).as_deref(),
                expected,
                "{override_cmd:?}"
            );
        }
    }

    #[test]
    fn resolve_sandbox_image_prefers_flag_then_merged_then_hardcoded() {
        for (flag, merged, expected) in [
            (Some(" custom:flag "), "repo:merged", "custom:flag"),
            (
                None,
                "ghcr.io/example/custom:latest",
                "ghcr.io/example/custom:latest",
            ),
            (None, "   ", HARDCODED),
            (None, "", HARDCODED),
        ] {
            assert_eq!(resolve_sandbox_image(flag, merged, HARDCODED), expected);
        }
    }

    mod generated_name {
        use super::profile_guard::dispatch_argv;
        use crate::git::test_support::{commit, run_git};
        use crate::session::{civilizations, Storage};
        use clap::Parser;
        use serial_test::serial;
        use std::path::{Path, PathBuf};

        fn init_repo_under(root: &Path) -> PathBuf {
            let profiles = crate::session::get_app_dir().unwrap().join("profiles");
            std::fs::create_dir_all(profiles.join("real")).unwrap();
            let repo_path = root.join("repo");
            std::fs::create_dir(&repo_path).unwrap();
            let repo = git2::Repository::init(&repo_path).unwrap();
            commit(&repo, Some("HEAD"), &[], &[], None);
            repo_path
        }

        fn only_session() -> crate::session::Instance {
            let (rows, _) = Storage::new_unwatched("real")
                .unwrap()
                .load_with_groups()
                .unwrap();
            assert_eq!(rows.len(), 1);
            rows.into_iter().next().unwrap()
        }

        #[test]
        fn flag_needs_a_worktree_and_excludes_an_explicit_title() {
            use crate::cli::Cli;
            for argv in [
                &["aoe", "add", ".", "--generated-name"][..],
                &["aoe", "add", ".", "-w", "b", "--generated-name", "-t", "x"][..],
            ] {
                assert!(Cli::try_parse_from(argv).is_err(), "{argv:?}");
            }
        }

        #[tokio::test]
        #[serial]
        async fn new_worktree_gets_a_generated_dir_and_title_but_keeps_the_branch() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let _tie = crate::session::test_support::TieWorkdirToNameGuard::set(false);
            let root = tempfile::tempdir().unwrap();
            let repo = init_repo_under(root.path());

            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                repo.to_str().unwrap(),
                "-w",
                "feat/my-feature",
                "-b",
                "--generated-name",
                "-p",
                "real",
            ]);
            super::super::run(&profile, args).await.unwrap();

            let session = only_session();
            assert!(civilizations::is_default_civ_name(&session.title));
            let info = session.worktree_info.expect("worktree session");
            assert_eq!(info.branch, "feat/my-feature");
            let leaf = crate::session::worktree_edit::worktree_leaf_from_title(&session.title);
            assert_eq!(
                Path::new(&session.project_path).canonicalize().unwrap(),
                root.path()
                    .join("repo-worktrees")
                    .join(leaf)
                    .canonicalize()
                    .unwrap()
            );
            assert_eq!(
                run_git(
                    Path::new(&session.project_path),
                    &["branch", "--show-current"]
                )
                .trim(),
                "feat/my-feature"
            );
        }

        fn title_for(
            repo: &Path,
            flags: &[&str],
            config: &crate::session::Config,
        ) -> anyhow::Result<String> {
            let mut argv = vec!["aoe", "add", repo.to_str().unwrap(), "--generated-name"];
            argv.extend_from_slice(flags);
            let (_, args) = dispatch_argv(&argv);
            super::super::generated_worktree_title(&args, repo, config, &[], "abcd1234")
        }

        #[test]
        #[serial]
        fn title_skips_names_whose_worktree_dir_exists() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let root = tempfile::tempdir().unwrap();
            let repo = init_repo_under(root.path());
            let free = civilizations::CIVILIZATIONS[0];
            let worktrees = root.path().join("repo-worktrees");
            for civ in &civilizations::CIVILIZATIONS[1..] {
                let leaf = crate::session::worktree_edit::worktree_leaf_from_title(civ);
                std::fs::create_dir_all(worktrees.join(leaf)).unwrap();
            }

            let title = title_for(
                &repo,
                &["-w", "feat/x", "-b"],
                &crate::session::Config::default(),
            )
            .unwrap();
            assert_eq!(title, free);
        }

        #[test]
        #[serial]
        fn session_id_template_with_an_existing_parent_still_generates_a_title() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let root = tempfile::tempdir().unwrap();
            let repo = init_repo_under(root.path());
            std::fs::create_dir(root.path().join("wt")).unwrap();
            let mut config = crate::session::Config::default();
            config.worktree.path_template = "../wt/{session-id}".to_string();

            title_for(&repo, &["-w", "feat/x", "-b"], &config)
                .expect("the occupied parent must not block a title");
        }

        #[test]
        #[serial]
        fn attaching_skips_the_directory_check() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let root = tempfile::tempdir().unwrap();
            let repo = init_repo_under(root.path());
            run_git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feat/x",
                    root.path().join("hand-made").to_str().unwrap(),
                ],
            );
            let worktrees = root.path().join("repo-worktrees");
            for civ in civilizations::CIVILIZATIONS {
                let leaf = crate::session::worktree_edit::worktree_leaf_from_title(civ);
                std::fs::create_dir_all(worktrees.join(leaf)).unwrap();
            }

            let title = title_for(&repo, &["-w", "feat/x"], &crate::session::Config::default())
                .expect("attaching creates no directory, so none can collide");
            assert!(civilizations::is_default_civ_name(&title));
        }

        #[tokio::test]
        #[serial]
        async fn attaching_keeps_the_existing_dir_and_generates_only_the_title() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let root = tempfile::tempdir().unwrap();
            let repo = init_repo_under(root.path());
            let existing = root.path().join("hand-made");
            run_git(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-b",
                    "feat/x",
                    existing.to_str().unwrap(),
                ],
            );

            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                repo.to_str().unwrap(),
                "-w",
                "feat/x",
                "--generated-name",
                "-p",
                "real",
            ]);
            super::super::run(&profile, args).await.unwrap();

            let session = only_session();
            assert!(civilizations::is_default_civ_name(&session.title));
            assert_eq!(
                Path::new(&session.project_path).canonicalize().unwrap(),
                existing.canonicalize().unwrap()
            );
            assert!(!root.path().join("repo-worktrees").exists());
        }

        #[tokio::test]
        #[serial]
        async fn blank_branch_is_refused_instead_of_adding_an_in_place_session() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let root = tempfile::tempdir().unwrap();
            let repo = init_repo_under(root.path());
            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                repo.to_str().unwrap(),
                "-w",
                " ",
                "--generated-name",
                "-p",
                "real",
            ]);
            let msg = super::super::run(&profile, args)
                .await
                .unwrap_err()
                .to_string();
            assert!(msg.contains("non-empty branch"), "{msg}");
            let (rows, _) = Storage::new_unwatched("real")
                .unwrap()
                .load_with_groups()
                .unwrap();
            assert!(rows.is_empty());
        }

        #[tokio::test]
        #[serial]
        async fn multi_repo_workspace_is_refused() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let root = tempfile::tempdir().unwrap();
            let repo = init_repo_under(root.path());
            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                repo.to_str().unwrap(),
                "-w",
                "feat/x",
                "-b",
                "--repo",
                root.path().join("other").to_str().unwrap(),
                "--generated-name",
                "-p",
                "real",
            ]);
            let msg = super::super::run(&profile, args)
                .await
                .unwrap_err()
                .to_string();
            assert!(msg.contains("--generated-name"), "{msg}");
        }
    }

    mod profile_guard {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        use serial_test::serial;

        pub(super) fn dispatch_argv(argv: &[&str]) -> (String, super::super::AddArgs) {
            let cli = Cli::try_parse_from(argv).expect("argv parses");
            let profile = cli.profile.unwrap_or_default();
            match cli.command {
                Some(Commands::Add(args)) => (profile, *args),
                _ => panic!("expected an add invocation"),
            }
        }

        /// After trust is decided, an adapter the first check let through for an on_create
        /// hook is refused when no such hook will run.
        #[test]
        #[serial]
        fn declined_trust_refuses_an_adapter_waiting_on_hooks() {
            use crate::session::config::repo_config::ResolvedHooks;
            let _home = crate::session::test_support::isolate_app_dir();
            let repo = tempfile::tempdir().unwrap();
            let installing = ResolvedHooks::with_repo(
                "default",
                repo.path(),
                crate::session::HooksConfig {
                    on_create: vec!["npm install".into()],
                    ..Default::default()
                },
            );
            let launching = ResolvedHooks::with_repo(
                "default",
                repo.path(),
                crate::session::HooksConfig {
                    on_launch: vec!["npm install".into()],
                    ..Default::default()
                },
            );
            assert!(installing.is_some() && launching.is_some());
            let refuse = super::super::refuse_adapter_without_hooks;
            assert!(refuse(None, None).is_ok(), "nothing waited on hooks");
            for (hook, resolved) in [("on_create", &installing), ("on_launch", &launching)] {
                assert!(
                    refuse(Some("my-acp"), resolved.as_ref()).is_ok(),
                    "an {hook} hook will run"
                );
            }
            let refused = refuse(Some("my-acp"), None).unwrap_err().to_string();
            assert!(
                refused.contains("`my-acp`") && refused.contains("--trust-hooks"),
                "{refused}"
            );
        }

        /// `aoe add` skips the host adapter check only for a sandbox the runtime can start,
        /// whichever way it was asked for, and otherwise judges the adapter as the create
        /// check does.
        #[test]
        #[serial]
        fn structured_adapter_check_follows_the_sandbox_and_the_agent_path() {
            let _home = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let bin = tempfile::tempdir().unwrap();
            let adapter = format!("aoe-test-add-adapter-{}", std::process::id());
            crate::session::test_support::write_stub_adapter(bin.path(), &adapter);
            let config = |tweak: &dyn Fn(&mut crate::session::Config)| {
                let mut config = crate::session::Config::default();
                tweak(&mut config);
                config
            };
            let plain = config(&|_| {});
            let on_path =
                config(&|c| c.environment = vec![format!("PATH={}", bin.path().display())]);
            let hooked = config(&|c| c.host_hooks.before_session = vec!["true".into()]);
            let sandbox_default = config(&|c| c.sandbox.enabled_by_default = true);
            let dir = project.path().to_str().unwrap();
            struct Case<'a> {
                label: &'static str,
                extra: &'a [&'a str],
                config: &'a crate::session::Config,
                runtime: bool,
                refused: bool,
            }
            let cases = [
                Case {
                    label: "found nowhere",
                    extra: &[],
                    config: &plain,
                    runtime: true,
                    refused: true,
                },
                Case {
                    label: "on the configured PATH",
                    extra: &[],
                    config: &on_path,
                    runtime: true,
                    refused: false,
                },
                Case {
                    label: "a before_session hook may set PATH",
                    extra: &[],
                    config: &hooked,
                    runtime: true,
                    refused: false,
                },
                Case {
                    label: "--sandbox",
                    extra: &["--sandbox"],
                    config: &plain,
                    runtime: true,
                    refused: false,
                },
                Case {
                    label: "--sandbox-image",
                    extra: &["--sandbox-image", "img:1"],
                    config: &plain,
                    runtime: true,
                    refused: false,
                },
                Case {
                    label: "sandbox on by default",
                    extra: &[],
                    config: &sandbox_default,
                    runtime: true,
                    refused: false,
                },
                Case {
                    label: "--sandbox without a runtime",
                    extra: &["--sandbox"],
                    config: &plain,
                    runtime: false,
                    refused: true,
                },
                Case {
                    label: "sandbox default without a runtime",
                    extra: &[],
                    config: &sandbox_default,
                    runtime: false,
                    refused: true,
                },
            ];
            for case in cases {
                let argv: Vec<&str> = ["aoe", "add", dir]
                    .iter()
                    .chain(case.extra)
                    .copied()
                    .collect();
                let (_, args) = dispatch_argv(&argv);
                let checked_runtime = std::cell::Cell::new(false);
                let got = super::super::structured_adapter_missing(
                    &args,
                    case.config,
                    &adapter,
                    project.path(),
                    || {
                        checked_runtime.set(true);
                        case.runtime
                    },
                    || false,
                );
                assert_eq!(got, case.refused, "{}", case.label);
                let asked_for_sandbox =
                    !case.extra.is_empty() || case.config.sandbox.enabled_by_default;
                assert_eq!(
                    checked_runtime.get(),
                    asked_for_sandbox,
                    "{}: runtime probe",
                    case.label
                );
            }
            // Check preparation hooks only when the adapter is missing.
            let (_, args) = dispatch_argv(&["aoe", "add", dir]);
            for (label, config, missing) in [
                ("found nowhere", &plain, true),
                ("on PATH", &on_path, false),
            ] {
                let asked = std::cell::Cell::new(false);
                let got = super::super::structured_adapter_missing(
                    &args,
                    config,
                    &adapter,
                    project.path(),
                    || false,
                    || {
                        asked.set(true);
                        true
                    },
                );
                assert!(!got, "{label}: an on_create hook may install it");
                assert_eq!(asked.get(), missing, "{label}: asked about hooks");
            }
        }

        /// `add` builds the fork child itself rather than through the session
        /// builder, so it needs its own identity comparison: a child asked for
        /// under another agent must not fork a conversation it cannot resume.
        /// A wrapper with no execution contract resolves to no agent, so the
        /// row names no conversation a fork could carry and is not a candidate.
        /// The REST election drops it; the CLI has to refuse it the same way
        /// rather than propagating the resolution error the other surface hides.
        #[tokio::test]
        #[serial]
        async fn add_refuses_an_unresolvable_parent_the_way_rest_drops_it() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let project = tempfile::tempdir().unwrap();
            let parent_id = "unresolvable-parent-uuid";
            crate::session::Storage::new_unwatched("real")
                .unwrap()
                .update(|rows, _| {
                    let mut parent =
                        crate::session::Instance::new("parent", project.path().to_str().unwrap());
                    parent.id = parent_id.to_string();
                    parent.tool = "claude".into();
                    parent.command = "ssh -t host claude".into();
                    parent.agent_session_id = Some("legacy-conversation-uuid".into());
                    parent.agent_session_binding = Some(
                        crate::session::ConversationBinding::unknown("legacy-conversation-uuid"),
                    );
                    *rows = vec![parent];
                    Ok(())
                })
                .unwrap();

            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                project.path().to_str().unwrap(),
                "--fork-from",
                parent_id,
                "-p",
                "real",
            ]);
            let msg = super::super::run(&profile, args)
                .await
                .expect_err("a parent whose agent cannot be resolved is not a candidate")
                .to_string();
            assert_eq!(
                msg,
                crate::session::ForkDenied::NoParentSession
                    .user_message("parent", parent_id, "real"),
                "the CLI and the REST election must refuse the row the same way"
            );
        }

        #[tokio::test]
        #[serial]
        async fn add_refuses_a_fork_child_under_another_agent() {
            let root = tempfile::tempdir().unwrap();
            let _guard = crate::session::test_support::isolate_app_dir_at(root.path());
            // The CLI resolves the requested tool's binary before the fork parent
            // check, so both agents need a command on the path.
            let _claude = crate::session::test_support::install_login_shell_path_command(
                root.path(),
                "claude",
                "#!/bin/sh\nexit 1\n",
            );
            let _codex = crate::session::test_support::install_login_shell_path_command(
                root.path(),
                "codex",
                "#!/bin/sh\nexit 1\n",
            );
            let project = tempfile::tempdir().unwrap();
            let parent_id = "parent-session-uuid";
            crate::session::Storage::new_unwatched("real")
                .unwrap()
                .update(|rows, _| {
                    let mut parent =
                        crate::session::Instance::new("parent", project.path().to_str().unwrap());
                    parent.id = parent_id.to_string();
                    parent.tool = "claude".into();
                    parent.command = "claude".into();
                    parent.agent_session_id = Some("legacy-conversation-uuid".into());
                    parent.agent_session_binding = Some(
                        crate::session::ConversationBinding::unknown("legacy-conversation-uuid"),
                    );
                    *rows = vec![parent];
                    Ok(())
                })
                .unwrap();

            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                project.path().to_str().unwrap(),
                "--fork-from",
                parent_id,
                "--tool",
                "codex",
                "-p",
                "real",
            ]);
            let msg = super::super::run(&profile, args)
                .await
                .expect_err("a fork under another agent must refuse")
                .to_string();
            assert!(
                msg.contains("codex") && msg.contains("claude"),
                "the refusal must name both agents, got: {msg}"
            );
        }

        /// `aoe add --fork-from --tool <account>` is the #4347 workflow: the
        /// child's own store must receive the parent's transcript and be the
        /// one its binding records.
        #[tokio::test]
        #[serial]
        async fn add_fork_carries_the_parent_onto_the_childs_store() {
            let root = tempfile::tempdir().unwrap();
            let _guard = crate::session::test_support::isolate_app_dir_at(root.path());
            let _claude = crate::session::test_support::install_login_shell_path_command(
                root.path(),
                "claude",
                "#!/bin/sh\nexit 1\n",
            );
            let home = dirs::home_dir().unwrap();
            let project = tempfile::tempdir().unwrap();
            let source = home.join("source");
            let sid = "11111111-2222-3333-4444-555555555555";
            let transcript = std::path::Path::new("projects")
                .join(crate::session::capture::encode_claude_project_path(
                    &project.path().to_string_lossy(),
                ))
                .join(format!("{sid}.jsonl"));
            std::fs::create_dir_all(source.join(transcript.parent().unwrap())).unwrap();
            std::fs::write(source.join(&transcript), "parent conversation\n").unwrap();
            let storage = crate::session::Storage::new_unwatched("real").unwrap();
            std::fs::write(
                crate::session::get_profile_dir("real")
                    .unwrap()
                    .join("config.toml"),
                "[session.custom_agents]\nclaude-b = \"claude\"\n\
                 [session.agent_detect_as]\nclaude-b = \"claude\"\n\
                 [session.agent_config_dir]\nclaude-b = \"~/destination\"\n",
            )
            .unwrap();
            let parent_id = "parent-session-uuid";
            storage
                .update(|rows, _| {
                    let mut parent =
                        crate::session::Instance::new("parent", project.path().to_str().unwrap());
                    parent.id = parent_id.to_string();
                    parent.tool = "claude".into();
                    parent.command = "claude".into();
                    parent.agent_session_id = Some(sid.into());
                    parent.agent_session_binding = Some(crate::session::ConversationBinding {
                        session_id: sid.into(),
                        provenance: crate::session::ConversationProvenance::Observed,
                        transcript_path: None,
                        execution: Some(crate::session::ExecutionBinding {
                            agent: "claude".into(),
                            stores: vec![source.clone()],
                            configuration: Vec::new(),
                            cwd: project.path().to_path_buf(),
                            cwd_filesystem: "host".into(),
                            filesystem: "host".into(),
                            exported_default_store: None,
                        }),
                    });
                    *rows = vec![parent];
                    Ok(())
                })
                .unwrap();

            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                project.path().to_str().unwrap(),
                "--fork-from",
                parent_id,
                "--tool",
                "claude-b",
                "-p",
                "real",
            ]);
            super::super::run(&profile, args).await.unwrap();

            let destination = home.join("destination");
            assert_eq!(
                std::fs::read_to_string(destination.join(&transcript)).unwrap(),
                "parent conversation\n"
            );
            let rows = storage.load().unwrap();
            let child = rows.iter().find(|row| row.tool == "claude-b").unwrap();
            assert_eq!(
                child
                    .resume_binding
                    .clone()
                    .unwrap()
                    .execution
                    .unwrap()
                    .stores,
                vec![destination.canonicalize().unwrap()]
            );
        }

        #[tokio::test]
        #[serial]
        async fn add_refuses_unknown_profile_before_vivifying_it() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let profiles = crate::session::get_app_dir().unwrap().join("profiles");
            std::fs::create_dir_all(profiles.join("real")).unwrap();

            let (profile, args) = dispatch_argv(&[
                "aoe",
                "add",
                "/nonexistent/aoe-add-path",
                "-p",
                "ghost-profile",
            ]);
            let msg = super::super::run(&profile, args)
                .await
                .expect_err("unknown profile must refuse `add`")
                .to_string();
            assert!(
                msg.contains("Profile 'ghost-profile' does not exist")
                    && msg.contains("aoe profile create ghost-profile"),
                "expected the unknown-profile error first, got: {msg}"
            );
            assert!(
                !profiles.join("ghost-profile").exists(),
                "`add -p <unknown>` must not mint profiles/ghost-profile"
            );
        }

        #[tokio::test]
        #[serial]
        async fn add_lets_a_known_profile_through_to_path_validation() {
            let _guard = crate::session::test_support::isolate_app_dir();
            let profiles = crate::session::get_app_dir().unwrap().join("profiles");
            std::fs::create_dir_all(profiles.join("real")).unwrap();

            let (profile, args) =
                dispatch_argv(&["aoe", "add", "/nonexistent/aoe-add-path", "-p", "real"]);
            let msg = super::super::run(&profile, args)
                .await
                .expect_err("the missing path is refused")
                .to_string();
            assert!(
                msg.contains("Path does not exist"),
                "a known profile must reach path validation, got: {msg}"
            );
        }
    }

    #[test]
    fn check_effort_request_gates_on_view_then_known_choices() {
        let known = ["low".to_string(), "high".to_string()];
        // (wants structured, choices, effort, error substring or warning substring)
        let cases: [(bool, &[String], &str, Result<Option<&str>, &str>); 5] = [
            (false, &known, "high", Err("structured view")),
            (true, &known, "high", Ok(None)),
            (true, &known, "ultra", Err("low, high")),
            (true, &[], "ultra", Ok(Some("unchecked"))),
            (true, &[], "high", Ok(Some("unchecked"))),
        ];
        for (structured, choices, effort, want) in cases {
            let got = check_effort_request(effort, structured, "codex", choices);
            match want {
                Ok(None) => assert_eq!(got.unwrap(), None, "{effort}"),
                Ok(Some(text)) => {
                    assert!(got.unwrap().is_some_and(|w| w.contains(text)), "{effort}")
                }
                Err(text) => {
                    let err = got.unwrap_err().to_string();
                    assert!(err.contains(text), "{err}");
                }
            }
        }
    }
}
