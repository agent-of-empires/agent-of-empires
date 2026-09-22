//! `agent-of-empires add` command implementation

use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::containers;
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

    /// Parent session (creates sub-session, inherits group)
    #[arg(short = 'P', long)]
    parent: Option<String>,

    /// Fork an existing session: resume its conversation context in a new,
    /// independent session that then diverges. Give the source session's id or
    /// title. Terminal fork; available for agents that support forking
    /// (claude, codex, opencode).
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

    /// Pick a specific ACP agent for the structured view (e.g., claude-code,
    /// codex).
    #[arg(long = "agent")]
    agent: Option<String>,

    /// Override the model used by the ACP agent (e.g., claude-opus-4-7,
    /// gpt-5, gemini-2.5-pro). Forwarded to the agent at session start.
    #[arg(long = "model")]
    model: Option<String>,

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

#[tracing::instrument(target = "cli.add", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: AddArgs) -> Result<()> {
    if args.interactive && !std::io::stdin().is_terminal() {
        bail!("--interactive requires a terminal; pass --title for non-interactive naming");
    }

    crate::session::require_known_profile(profile)?;

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

    if (!args.extra_repos.is_empty() || !args.projects.is_empty())
        && explicit_worktree_branch(&args).is_none()
    {
        bail!("--repo/--project requires --worktree to specify a branch\nTip: aoe add /path --project repoB -w branch-name");
    }

    if !args.repo_bases.is_empty() && explicit_worktree_branch(&args).is_none() {
        bail!("--repo-base requires --worktree to specify a branch\nTip: aoe add /path --project repoB -w branch-name --repo-base repoB=develop");
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
    let final_title = resolve_session_title(&args, &instances)?;

    let mut resolved_tool = resolve_tool_for_add(&args, &config)?;

    let wants_structured = args.structured_view || args.agent.is_some();
    if args.fork_from.is_some() && wants_structured {
        bail!(
            "`--fork-from` performs a terminal fork and cannot be combined with \
             --structured-view or --agent; structured fork is available from the web dashboard."
        );
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
        let collides_with_fork_flags = |cmd: &str| {
            cmd.split_whitespace().any(|w| {
                w == "resume"
                    || w == "fork"
                    || w.starts_with("--resume")
                    || w.starts_with("--session")
                    || w.starts_with("--fork")
            })
        };
        for input in [args.command.as_deref(), args.extra_args.as_deref()]
            .into_iter()
            .flatten()
        {
            if collides_with_fork_flags(input) {
                bail!(
                    "`--fork-from` cannot be combined with a launch command (--cmd or --extra-args) \
                     that already contains a resume or fork flag/subcommand: the fork appends its \
                     own resume flags, which would collide."
                );
            }
        }
    }

    let fork_seed: Option<crate::session::ForkSeed> = if let Some(fork_ref) = &args.fork_from {
        let source = super::resolve_session(fork_ref, &instances)?;
        if matches!(
            source.resume_intent,
            crate::session::ResumeIntent::Fork { .. }
        ) {
            bail!(
                "Cannot fork from session '{}': its own fork has not launched yet. Start it once, then fork from the child conversation.",
                source.title
            );
        }
        let user_chose_tool = args.tool.is_some() || args.command.is_some();
        if user_chose_tool && resolved_tool != source.tool {
            bail!(
                "Cannot fork session '{}' (agent '{}') as agent '{}': a fork must use the parent's \
                 agent. Drop --tool/--cmd to inherit it, or fork a session created with '{}'.",
                source.title,
                source.tool,
                resolved_tool,
                resolved_tool
            );
        }
        if !user_chose_tool {
            resolved_tool = source.tool.clone();
        }
        let parent_agent_session_id = source.agent_session_id.clone();
        let seed = crate::session::fork::terminal_fork_seed(
            &resolved_tool,
            parent_agent_session_id.as_deref(),
            crate::session::capture::generate_session_uuid(),
        )
        .map_err(|denied| match denied {
            crate::session::ForkDenied::AgentCannotFork => anyhow::anyhow!(
                "Agent '{}' does not support forking. Forkable agents: claude, codex, opencode.",
                resolved_tool
            ),
            crate::session::ForkDenied::NoParentSession => anyhow::anyhow!(
                "Nothing to fork: session '{}' has no captured agent session yet. Start a conversation in it first.",
                source.title
            ),
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
                        &path.to_string_lossy(),
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

            let attach_existing = !args.create_branch;
            let existing_match = if attach_existing {
                git_wt.list_worktrees().ok().and_then(|wts| {
                    wts.into_iter()
                        .find(|wt| wt.branch.as_deref() == Some(branch))
                })
            } else {
                None
            };

            if let Some(existing) = existing_match {
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
                let session_id = uuid::Uuid::new_v4().to_string();
                let session_id_short = &session_id[..8];

                let template = if GitWorktree::is_bare_repo(&main_repo_path) {
                    &config.worktree.bare_repo_path_template
                } else {
                    &config.worktree.path_template
                };
                let leaf_seed_owned;
                let leaf_seed = if config.session.tie_workdir_to_name {
                    leaf_seed_owned =
                        crate::session::worktree_edit::worktree_leaf_from_title(&final_title);
                    leaf_seed_owned.as_str()
                } else {
                    branch
                };
                let worktree_path = git_wt.compute_path(leaf_seed, template, session_id_short)?;

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

    instance.yolo_mode = args.yolo || config.session.yolo_mode_default;

    if let Some(ref extra) = args.extra_args {
        instance.extra_args = extra.clone();
    } else if let Some(extra) = config.session.agent_extra_args.get(&instance.tool) {
        if !extra.is_empty() {
            instance.extra_args = extra.clone();
        }
    }

    if let Some(ref cmd) = args.cmd_override {
        instance.command = cmd.clone();
    } else {
        let resolved = config.session.resolve_tool_command(&instance.tool);
        if !resolved.is_empty() {
            instance.command = resolved;
        }
    }

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
        if !crate::cli::acp::command_present(&spec.command) {
            let hint = crate::acp::install_hints::install_hint_for(&spec.command)
                .unwrap_or("install via your package manager and re-run");
            if user_picked_agent {
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

    if instance.is_structured() {
        let defaults = config.acp.acp_defaults_for(&agent_name);
        instance.agent_model = crate::session::config::resolve_spawn_model_effort(
            defaults,
            instance.agent_model.take(),
            None,
        )
        .0;
    }

    if let Some(seed) = fork_seed {
        match seed {
            crate::session::ForkSeed::Terminal {
                parent_agent_session_id,
                child_session_id,
            } => {
                instance.agent_session_id = Some(child_session_id);
                instance.resume_intent = crate::session::ResumeIntent::Fork {
                    from: parent_agent_session_id,
                };
            }
            crate::session::ForkSeed::Structured { .. } => {}
        }
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
            for w in crate::session::validate_env_entries(&config.sandbox.environment) {
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
                container_workdir: None,
            });
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
                        match &trust.hooks {
                            TrustSurface::Trusted(h) => {
                                repo_config::ResolvedHooks::with_repo(profile, repo_root, h.clone())
                            }
                            TrustSurface::NeedsTrust { .. } => None,
                            TrustSurface::Absent => repo_config::ResolvedHooks::global(profile),
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "cli.add", "Failed to check repo trust: {}", e);
                    repo_config::ResolvedHooks::global(profile)
                }
            }
        };

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
        Ok(true) => {}
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

fn resolve_session_title(args: &AddArgs, instances: &[Instance]) -> Result<String> {
    if let Some(title) = &args.title {
        return Ok(title.trim().to_string());
    }
    let default_title = if let Some(branch) = explicit_worktree_branch(args) {
        branch.to_string()
    } else {
        let existing_titles: Vec<&str> = instances.iter().map(|i| i.title.as_str()).collect();
        civilizations::generate_random_title(&existing_titles)
    };
    if args.interactive {
        prompt_session_title(&default_title)
    } else {
        Ok(default_title)
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
    use super::{override_launch_binary, parse_repo_base, resolve_sandbox_image};
    use crate::session::config::SessionConfig;

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
    fn override_launch_binary_uses_command_override() {
        let mut session = SessionConfig::default();
        session
            .agent_command_override
            .insert("opencode".to_string(), "opencode-plannotator".to_string());
        assert_eq!(
            override_launch_binary("opencode", &session).as_deref(),
            Some("opencode-plannotator")
        );
    }

    #[test]
    fn override_launch_binary_takes_first_word_of_multiword_override() {
        let mut session = SessionConfig::default();
        session
            .agent_command_override
            .insert("opencode".to_string(), "ocp run sp".to_string());
        assert_eq!(
            override_launch_binary("opencode", &session).as_deref(),
            Some("ocp")
        );
    }

    #[test]
    fn override_launch_binary_honors_quoted_path() {
        let mut session = SessionConfig::default();
        session.agent_command_override.insert(
            "opencode".to_string(),
            "\"/opt/My Wrapper/opencode\" --mode plan".to_string(),
        );
        assert_eq!(
            override_launch_binary("opencode", &session).as_deref(),
            Some("/opt/My Wrapper/opencode")
        );
    }

    #[test]
    fn override_launch_binary_none_without_override() {
        let session = SessionConfig::default();
        assert_eq!(override_launch_binary("opencode", &session), None);
    }

    #[test]
    fn flag_overrides_everything() {
        let image = resolve_sandbox_image(Some(" custom:flag "), "repo:merged", HARDCODED);
        assert_eq!(image, "custom:flag");
    }

    #[test]
    fn merged_default_used_when_no_flag() {
        let image = resolve_sandbox_image(None, "ghcr.io/example/custom:latest", HARDCODED);
        assert_eq!(image, "ghcr.io/example/custom:latest");
    }

    #[test]
    fn whitespace_merged_falls_back_to_hardcoded() {
        let image = resolve_sandbox_image(None, "   ", HARDCODED);
        assert_eq!(image, HARDCODED);
    }

    #[test]
    fn empty_merged_falls_back_to_hardcoded() {
        let image = resolve_sandbox_image(None, "", HARDCODED);
        assert_eq!(image, HARDCODED);
    }

    mod profile_guard {
        use crate::cli::{Cli, Commands};
        use clap::Parser;
        use serial_test::serial;

        fn dispatch_argv(argv: &[&str]) -> (String, super::super::AddArgs) {
            let cli = Cli::try_parse_from(argv).expect("argv parses");
            let profile = cli.profile.unwrap_or_default();
            match cli.command {
                Some(Commands::Add(args)) => (profile, *args),
                _ => panic!("expected an add invocation"),
            }
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
}
