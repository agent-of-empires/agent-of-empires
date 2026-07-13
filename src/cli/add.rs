//! `agent-of-empires add` command implementation

use anyhow::{bail, Context, Result};
use clap::Args;
use std::io::IsTerminal;
use std::path::PathBuf;

use crate::containers;
use crate::session::builder;
use crate::session::repo_config;
use crate::session::{civilizations, GroupTree, Instance, SandboxInfo, Storage};

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
    #[cfg(feature = "serve")]
    #[arg(long = "structured-view")]
    structured_view: bool,

    /// Pick a specific ACP agent for the structured view (e.g., aoe-agent,
    /// claude-code).
    #[cfg(feature = "serve")]
    #[arg(long = "agent")]
    agent: Option<String>,

    /// Override the model used by aoe-agent (e.g., claude-opus-4-7,
    /// gpt-5, gemini-2.5-pro). Forwarded to the agent at session start.
    #[cfg(feature = "serve")]
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
            "extra_repos",
            "projects",
            "no_submodules",
        ]
    )]
    scratch: bool,
}

#[tracing::instrument(target = "cli.add", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: AddArgs) -> Result<()> {
    // Fail fast before any filesystem side effects: --interactive must
    // have a real terminal to read the name from, otherwise the prompt
    // would block on EOF or a PTY harness would hang.
    if args.interactive && !std::io::stdin().is_terminal() {
        bail!("--interactive requires a terminal; pass --title for non-interactive naming");
    }

    // Scratch sessions have no project path; the scratch directory is
    // provisioned below once we know the instance id. Reject an
    // explicitly-passed path loudly so `aoe add /some/repo --scratch` does
    // not silently drop the path arg.
    if args.scratch && args.path.is_some() {
        bail!(
            "Cannot specify a project path with --scratch\nTip: drop the path argument, the session runs in a fresh scratch directory"
        );
    }

    let mut path = if args.scratch {
        // Placeholder; the real path is set after `Instance::new` runs and
        // `scratch::provision_scratch_dir` returns a fresh scratch dir.
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

    // Scratch sessions have no project repo, so repo-scoped config
    // overrides have nothing to anchor on. Resolving the repo-aware
    // variant against the launch directory would silently pick up
    // `.agent-of-empires/config.toml` from whatever folder the user
    // happened to run `aoe add --scratch` in, which breaks the
    // project-less contract. Fall back to the profile-only resolver.
    let config = if args.scratch {
        crate::session::profile_config::resolve_config_or_warn(profile)
    } else {
        repo_config::resolve_config_with_repo_or_warn(profile, &path)
    };

    // Preserve the original project path for hook trust checking.
    // `path` gets reassigned to the worktree/workspace directory below,
    // but hooks are defined in the original repo's `.agent-of-empires/config.toml`.
    let original_project_path = path.clone();

    let mut worktree_info_opt = None;
    let mut workspace_info_opt = None;

    // Phase 1 (unlocked): pre-flight read of the current persisted state to
    // resolve `--parent`, generate a non-colliding title, and make best-effort
    // duplicate / parent decisions before any side effects. Final duplicate
    // enforcement happens under the flock in phase 3.
    //
    // The title is resolved here, before worktree creation, so a tied worktree
    // session (`session.tie_workdir_to_name`) can seed its directory leaf from
    // the title and start out aligned (#1927). The path-dependent duplicate
    // check still runs later, once `path` points at the worktree.
    let storage = Storage::new_unwatched(profile)?;
    let (instances, _groups) = storage.load_with_groups()?;
    let final_title = resolve_session_title(&args, &instances)?;

    // Resolve the agent tool now, before any worktree/scratch side effects.
    // The fork-eligibility gate keys off the resolved tool (and the source
    // session's captured agent id), and both are knowable here: resolving and
    // validating before resource creation means an unforkable agent or a
    // parent with no captured session bails without orphaning a worktree or
    // scratch directory. The instance does not exist yet, so the seed is held
    // and applied once the instance is built.
    // `mut` because a `--fork-from` with no explicit `--tool`/`--cmd` inherits
    // the parent's agent below.
    let mut resolved_tool = resolve_tool_for_add(&args, &config)?;

    // `--fork-from` performs a TERMINAL fork (it seeds `agent_session_id` + a
    // one-shot Fork resume intent). Pairing it with a structured-view request
    // would write that terminal state onto a structured session, which is
    // incoherent: structured fork is its own flow (ACP `session/fork`) and is
    // offered from the web dashboard, not here. Reject it here, before any
    // worktree or scratch directory is created, so the refusal leaks nothing.
    // The structured-view flags are serve-gated, so `wants_structured` is
    // always false on bare-core.
    #[cfg(feature = "serve")]
    let wants_structured = args.structured_view || args.agent.is_some();
    #[cfg(not(feature = "serve"))]
    let wants_structured = false;
    if args.fork_from.is_some() && wants_structured {
        bail!(
            "`--fork-from` performs a terminal fork and cannot be combined with \
             --structured-view or --agent; structured fork is available from the web dashboard."
        );
    }

    // A terminal fork resumes the parent's captured conversation in place: the
    // agent finds that conversation by session id under the SAME working
    // directory and filesystem view. Flags that move the cwd (`--worktree` /
    // `--new-branch` / `--scratch`) or swap the filesystem (`--sandbox` /
    // `--sandbox-image`) silently break that lookup, and a user-supplied launch
    // command carrying its own resume/fork flags collides with the ones the
    // Fork intent appends. Reject these up front (before any resource creation)
    // rather than launch a fork that can't find its parent. See PR review.
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
        // --cmd-override swaps the launched binary out from under the tool: the
        // Fork intent builds its resume+fork flags for `instance.tool`, but the
        // override binary may be a different agent that rejects them (or, worse,
        // a different agent handed the parent's agent-shaped id). Reject the
        // pair rather than launch a cross-agent fork the tool check can't see.
        if args.cmd_override.is_some() {
            bail!(
                "`--fork-from` cannot be combined with --cmd-override: overriding the agent binary \
                 decouples it from the parent's agent, so the fork's resume flags may not apply."
            );
        }
        // The Fork intent appends the agent's own resume+fork flags: claude
        // `--resume`/`--session-id`/`--fork-session`, opencode `--session`/
        // `--fork`, codex `resume`/`fork` subcommands. A launch command that
        // already carries any of them produces a duplicate/conflicting
        // invocation. Match at WORD granularity (not raw substring) so a path
        // or unrelated arg containing "fork"/"resume" (e.g. `--model resume-v2`
        // or `/src/fork-utils`) doesn't false-trip, while `--session=ID` and the
        // codex `fork`/`resume` subcommands still do.
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

    // Validate fork eligibility eagerly and produce the one-shot seed. This is
    // a pure decision (source-session lookup over the already-loaded
    // `instances`, plus `terminal_fork_seed`, which only consults the agent's
    // static fork strategy), so it is safe to run before resource creation.
    let fork_seed: Option<crate::session::ForkSeed> = if let Some(fork_ref) = &args.fork_from {
        let source = super::resolve_session(fork_ref, &instances)?;
        // A source that was itself created as a fork and has not launched yet
        // still carries a one-shot Fork intent, and its `agent_session_id` is a
        // pre-pinned child id that no agent has written to disk. Forking from it
        // would resume a conversation that does not exist. Refuse until the
        // child has run once and owns a real captured id.
        if matches!(
            source.resume_intent,
            crate::session::ResumeIntent::Fork { .. }
        ) {
            bail!(
                "Cannot fork from session '{}': its own fork has not launched yet. Start it once, then fork from the child conversation.",
                source.title
            );
        }
        // The child must fork the SAME agent as the parent: a captured id is
        // agent-shaped (a Claude UUID resumes only under Claude, etc.), so
        // handing it to a different agent's `--resume` fails or resumes garbage.
        // When the user did not explicitly choose a tool (`--tool`/`--cmd`),
        // inherit the parent's; when they did and it differs, reject rather than
        // launch a cross-agent fork.
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
            crate::session::capture::generate_claude_session_id(),
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

            // The launch repo never consults the per-project layer: explicit
            // session base, then the global/profile default.
            let primary = builder::WorkspaceRepoSpec {
                base_branch: builder::resolve_base_branch(session_base, None, global_default),
                path: path.clone(),
            };
            let extra_repos: Vec<builder::WorkspaceRepoSpec> = all_extra_repos
                .iter()
                .map(|p| builder::WorkspaceRepoSpec {
                    base_branch: resolve_extra(p),
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
            // Single worktree mode (existing logic)
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

            // Attach mode: when `-b` is not passed, mirror the TUI's "Attach
            // to existing branch" behavior. If a worktree already exists
            // for this branch, point the session at it instead of bailing.
            // This closes the CLI half of #969 / matches builder.rs.
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

                // Choose appropriate template based on repo type (bare vs regular)
                // Use main_repo_path (not path) to correctly detect bare repos when running from a worktree
                let template = if GitWorktree::is_bare_repo(&main_repo_path) {
                    &config.worktree.bare_repo_path_template
                } else {
                    &config.worktree.path_template
                };
                // Tied sessions name the directory after the title, not the
                // branch, so the two cannot drift. The branch still creates the
                // worktree below; only the path leaf changes. (#1927)
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
                // Single-repo sessions only have the launch repo, so fall back
                // from the explicit session base to the global/profile default.
                let base = if args.create_branch {
                    builder::resolve_base_branch(
                        args.base_branch.as_deref(),
                        None,
                        config.worktree.default_base_branch.as_deref(),
                    )
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

    // Resolve parent session if specified
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

    // The title was resolved before worktree creation (so a tied session could
    // seed its directory leaf from it); run the path-dependent duplicate check
    // now that `path` points at the final worktree/workspace directory.
    if is_duplicate_session(&instances, &final_title, path.to_str().unwrap_or("")) {
        println!(
            "Session already exists with same title and path: {}",
            final_title
        );
        cleanup_partial_session(
            &path,
            worktree_info_opt.as_ref(),
            workspace_info_opt.as_ref(),
            args.create_branch,
            None,
            None,
        );
        return Ok(());
    }

    let mut instance = Instance::new(&final_title, path.to_str().unwrap_or(""));
    instance.source_profile = profile.to_string();

    // Scratch sessions: provision a fresh scratch directory keyed on the
    // freshly-generated instance id. The session layer owns the location
    // (`<app_dir>/scratch/<id>/`) and the deletion guard.
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

    // Tool name was resolved before worktree/scratch creation (so the fork
    // gate could bail early without orphaning resources); assign it here.
    instance.tool = resolved_tool;
    // Only store a custom command when the user passed extra args via --cmd
    // (e.g. "claude --resume xyz"). A bare tool name/alias should resolve
    // through the agent definition so the correct binary is used.
    if let Some(cmd) = &args.command {
        if cmd.trim().contains(' ') {
            instance.command = cmd.clone();
        }
    }

    // Set detect_as for status detection (resolved once, avoids config load in poll loop)
    instance.detect_as = config
        .session
        .agent_detect_as
        .get(&instance.tool)
        .cloned()
        .unwrap_or_default();

    // Apply set_default_command for agents that need it (e.g., opencode, codex)
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

    // Apply extra_args and command override: CLI flags take priority, then config defaults
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

    // View selection. The terminal view (raw tmux/PTY) is the default so the
    // CLI matches the TUI; the web wizard is the surface that defaults to
    // structured. `--structured-view` (or `--agent`, which names a specific
    // ACP agent) opts into the structured rendering; a non-ACP tool always
    // runs in the terminal view.
    #[cfg(feature = "serve")]
    {
        // `--agent` is an explicit structured-view choice: the user named a
        // specific ACP agent, so a missing adapter is a hard error rather
        // than a silent downgrade.
        let user_picked_agent = args.agent.is_some();
        let user_wants_structured = args.structured_view || user_picked_agent;
        // The `--fork-from` + structured-view refusal is hoisted above
        // worktree/scratch creation (see the early fork-validation block) so it
        // leaks no resources; nothing to re-check here.
        instance.agent_name = args.agent.clone();
        instance.agent_model = args.model.clone();

        let registry = crate::acp::agent_registry::AgentRegistry::with_defaults();
        let agent_name = pick_acp_agent_name(
            &registry,
            &config.session,
            &instance.tool,
            instance.agent_name.as_deref(),
        );
        // Capability is judged against the explicit `--agent` (or, with none,
        // the tool itself), NOT `pick_acp_agent_name`'s aoe-agent fallback:
        // otherwise every tool would look ACP-capable via the bundled default
        // and `--structured-view` could never be rejected for a non-ACP tool
        // (it would silently substitute aoe-agent). Mirrors the server create
        // path in `src/server/api/sessions.rs`.
        let capability_key = instance
            .agent_name
            .as_deref()
            .unwrap_or(instance.tool.as_str());
        let acp_capable = registry.get(capability_key).is_some()
            || config.session.agent_acp_cmd.contains_key(capability_key)
            || config.session.agent_acp_cmd.contains_key(&instance.tool);

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

        // Precondition: the structured view needs the resolved ACP adapter
        // binary on PATH. A missing adapter would otherwise surface as a
        // silent 404 on the first prompt. When the user explicitly named
        // an agent (--agent) we bail; otherwise (the default path) we fall
        // back to the terminal view with a warning so `aoe add` still
        // succeeds on a machine without the adapter installed.
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
                        None => unreachable!("acp_capable implies a resolvable spec"),
                    },
                },
            };
            // Overlay session.agent_command_override the same way the agent
            // spawn path does, so the precondition checks the binary that
            // will actually launch (e.g. opencode-plannotator), not the
            // bare registry binary. See #1910.
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
                         Or use the bundled fallback: rerun with `--agent aoe-agent`\n\
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

        // Pin the structured-view model AFTER the adapter check above, which may
        // have downgraded the session to terminal. Only a session that stays
        // structured persists the per-agent default: agent_model is ACP-only, so
        // a terminal fallback must not retain an ACP-derived default. Routed
        // through the shared resolver so an explicit --model wins and is trimmed
        // identically to the web create path; an explicit --model on a
        // downgraded session is left untouched. `agent_name` is the same key the
        // spawn resolves defaults against (see pick_agent_for_tool). Effort has
        // no Instance field, so the spawn path resolves the default effort.
        if instance.is_structured() {
            let defaults = config.acp.acp_defaults_for(&agent_name);
            instance.agent_model = crate::session::config::resolve_spawn_model_effort(
                defaults,
                instance.agent_model.take(),
                None,
            )
            .0;
        }
    }

    // Apply the fork seed validated earlier (before worktree/scratch creation):
    // pre-pin the child agent id and set the one-shot Fork intent, mirroring the
    // builder's Terminal arm. Validating up front and mutating here keeps the
    // eligibility error from orphaning a worktree or scratch dir.
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
            crate::session::ForkSeed::Structured { .. } => {
                // Terminal fork only from the CLI; nothing to apply.
            }
        }
    }

    // Handle sandbox setup
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
            // Surface env-resolution warnings before container creation so
            // typos and missing host vars don't silently produce empty
            // values inside the sandbox. Same source the TUI path uses.
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

    // Check for repository hooks.
    // Use the original project path for trust checking (not the worktree/workspace
    // path, which won't contain `.agent-of-empires/config.toml`).
    let hook_result: Result<()> = (|| {
        let resolved_hooks: Option<crate::session::HooksConfig> = if args.scratch {
            // Scratch sessions never have a `.agent-of-empires/config.toml`
            // anchored on `original_project_path` (the path is either
            // empty or the scratch dir itself). Skip the repo hook
            // trust prompt entirely and fall back to profile-level
            // hooks so the project-less contract stays intact.
            repo_config::resolve_global_profile_hooks(profile)
        } else {
            // Repo trust now covers two surfaces: lifecycle hooks and project
            // MCP servers (#1985). Hooks run here at create time; project MCP is
            // forwarded later by the daemon, but its trust is recorded through
            // the same single approval so an untrusted repo's `.mcp.json` is
            // never forwarded. Hooks are resolved independently of MCP so an
            // unapproved (or absent) MCP file never suppresses trusted hooks.
            use repo_config::TrustSurface;
            match repo_config::check_repo_trust(&original_project_path) {
                Ok(trust) => {
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
                            Some(h) => repo_config::merge_hooks_with_config(profile, h),
                            None => repo_config::resolve_global_profile_hooks(profile),
                        }
                    } else {
                        println!(
                            "Skipped (session created without trusting repo hooks or project MCP)"
                        );
                        // Already-trusted hooks still run; only newly-prompted
                        // surfaces are declined.
                        match &trust.hooks {
                            TrustSurface::Trusted(h) => {
                                repo_config::merge_hooks_with_config(profile, h.clone())
                            }
                            TrustSurface::NeedsTrust { .. } => None,
                            TrustSurface::Absent => {
                                repo_config::resolve_global_profile_hooks(profile)
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "cli.add", "Failed to check repo trust: {}", e);
                    repo_config::resolve_global_profile_hooks(profile)
                }
            }
        };

        if let Some(hooks) = resolved_hooks {
            if !hooks.on_create.is_empty() {
                // Show the final merged hook list (repo hooks override global/profile
                // per type) so the user can see exactly what runs, especially when
                // `--trust-hooks` skipped the interactive approval prompt (#596).
                println!("Running on_create hooks:");
                for cmd in &hooks.on_create {
                    println!("  {}", cmd);
                }
                let hook_env = repo_config::lifecycle_env_vars(&instance);
                if instance.sandbox_info.is_some() {
                    instance.get_container_for_instance()?;
                    let workdir = instance.container_workdir();
                    if let Some(ref sandbox) = instance.sandbox_info {
                        repo_config::execute_hooks_in_container(
                            &hooks.on_create,
                            &sandbox.container_name,
                            &workdir,
                            &hook_env,
                        )?;
                    }
                } else {
                    repo_config::execute_hooks(&hooks.on_create, &path, &hook_env)?;
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

    let persist_result = storage.update(|all_instances, groups| {
        if is_duplicate_session(
            all_instances,
            &instance.title,
            instance.project_path.as_str(),
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
            println!(
                "Session already exists with same title and path: {}",
                instance.title
            );
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
            return Ok(());
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

    #[cfg(feature = "serve")]
    let is_acp = instance.is_structured();
    #[cfg(not(feature = "serve"))]
    let is_acp = false;

    if is_acp {
        // Acp sessions aren't backed by tmux: their ACP worker is
        // owned by `aoe serve`'s supervisor, which the
        // status_poll_loop reconciler auto-spawns within ~2s of the
        // session appearing on disk. `--launch` and the
        // `aoe session start` next-step would both no-op (or now
        // bail), so route the user to the dashboard instead.
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
        // Persist Status::Error + last_error on launch failure rather than
        // cleanup_partial_session: row is committed; surface as broken.
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
                tmux_session.attach()?;
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
        println!("  aoe session start {}   # Start the session", final_title);
        println!("  aoe                         # Open TUI and press Enter to attach");
    }

    Ok(())
}

/// Prompt for a session title on stderr, mirroring the TUI `n` flow's
/// "auto-generates if empty" field. Empty input or EOF keeps
/// `default_title`; a non-empty line is trimmed and used. Only reached in
/// `--interactive` mode, which already verified stdin is a terminal.
/// Resolve the session title string (no path-dependent duplicate check).
///
/// `--title` wins; otherwise the default is the worktree branch name, or a
/// random civilization name for non-worktree sessions. `--interactive` prompts
/// with that default prefilled. Resolved before worktree creation so a tied
/// session can derive its directory leaf from the title (#1927); the duplicate
/// check runs later once the worktree path is known.
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
    // Tear down the sandbox container first so its bind mount releases the
    // worktree before the git removal below. Best-effort and idempotent: a
    // container that was never started yields ContainerNotFound, which is not
    // an error. `Some` only when the session is sandboxed.
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
    // Remove the scratch directory provisioned earlier in this run.
    // Guarded by `is_scratch_path` (same check the deletion path uses),
    // so a tampered or unexpected `project_path` is a no-op.
    if let Some(scratch) = scratch_dir {
        if crate::session::scratch::is_scratch_path(scratch) {
            let _ = std::fs::remove_dir_all(scratch);
        }
    }
}

pub fn is_duplicate_session(instances: &[Instance], title: &str, path: &str) -> bool {
    let normalized_path = path.trim_end_matches('/');
    instances.iter().any(|inst| {
        let existing_path = inst.project_path.trim_end_matches('/');
        existing_path == normalized_path && inst.title == title
    })
}

/// Sync mirror of `Supervisor::pick_agent_for_tool` so add-time
/// precondition checks can resolve the agent without spinning up the
/// async supervisor. Precedence: explicit override → tool-keyed
/// registry entry → custom agent with `agent_acp_cmd` → legacy
/// (`claude` → `claude`, else `aoe-agent`).
#[cfg(feature = "serve")]
fn pick_acp_agent_name(
    registry: &crate::acp::agent_registry::AgentRegistry,
    session: &crate::session::config::SessionConfig,
    tool: &str,
    explicit_override: Option<&str>,
) -> String {
    if let Some(name) = explicit_override {
        if !name.is_empty() {
            return name.to_string();
        }
    }
    if registry.get(tool).is_some() {
        return tool.to_string();
    }
    if session.agent_acp_cmd.contains_key(tool) {
        return tool.to_string();
    }
    if tool == "claude" {
        "claude".into()
    } else {
        "aoe-agent".into()
    }
}

/// Resolve the agent tool name a new session will run, performing the same
/// PATH-availability and conflict checks the create flow needs, with no
/// filesystem side effects. Resolved before worktree/scratch creation so the
/// fork-eligibility gate can bail early without leaving an orphaned worktree
/// or scratch dir behind; the resolved name is then assigned to the instance.
///
/// Precedence mirrors the inline create flow: explicit `--tool`, then
/// `--cmd`, then the resolved config default / first available tool / claude.
fn resolve_tool_for_add(args: &AddArgs, config: &crate::session::Config) -> Result<String> {
    if let Some(tool) = &args.tool {
        let selection = resolve_named_tool(tool, config)?;
        if selection.is_custom() && args.cmd_override.is_some() {
            bail!("--cmd-override cannot be used with configured custom agent --tool selections");
        }
        Ok(selection.name().to_string())
    } else if let Some(cmd) = &args.command {
        let tool_name = detect_tool(cmd)?;
        // Verify the binary that will actually launch is on PATH before
        // creating the session. A configured session.agent_command_override
        // (or custom_agents) entry replaces the built-in binary, so check the
        // resolved command, not the built-in name, otherwise `--cmd opencode`
        // falsely bails when only the override binary (e.g.
        // opencode-plannotator) is installed. See #1910.
        match override_launch_binary(&tool_name, &config.session) {
            Some(bin) => {
                // Use the same detection as tmux (login-shell PATH fallback
                // included) so an override binary visible only after shell
                // init isn't rejected here while the non-override path accepts
                // it. See #1910.
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
        Ok(tool_name)
    } else {
        // Use default_tool from resolved config, then first available tool, then "claude".
        // Check custom_agents first (exact match) before resolve_tool_name (substring match),
        // so names like "lenovo-claude" resolve as the custom agent, not built-in "claude".
        let available_tools = crate::tmux::AvailableTools::detect();
        let tools_list = available_tools.available_list();
        Ok(config
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
            .to_string())
    }
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

/// The binary `aoe add` must verify is on PATH for a `--cmd <tool>`
/// selection when `session.agent_command_override` (or `custom_agents`)
/// remaps the built-in to a different command. Returns the resolved
/// command's first word, or `None` when no override applies (the caller
/// then falls back to the built-in agent's own detection). See #1910.
///
/// Parsed with `shell_words` so a quoted path (e.g.
/// `"/opt/My Wrapper/opencode" --mode plan`) yields the real binary, matching
/// how `apply_agent_command_override` splits the command at spawn time.
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

/// Resolve the sandbox image for a new session.
///
/// Precedence: the explicit `--sandbox-image` flag, then the merged
/// `[sandbox] default_image` from `config` (which `resolve_config_with_repo_or_warn`
/// already layers repo over profile/global, see #1651), then the runtime's
/// hardcoded default. The merged value already carries the global config, so
/// there is no need to reload it from disk for the empty-fallback case.
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
    use super::{override_launch_binary, resolve_sandbox_image};
    use crate::session::config::SessionConfig;

    const HARDCODED: &str = "ghcr.io/agent-of-empires/aoe-sandbox:latest";

    #[test]
    fn override_launch_binary_uses_command_override() {
        let mut session = SessionConfig::default();
        session
            .agent_command_override
            .insert("opencode".to_string(), "opencode-plannotator".to_string());
        // The gate must verify the override binary, not the built-in
        // `opencode`, so `--cmd opencode` works when only the wrapper is
        // installed. See #1910.
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
        // shell_words keeps the quoted path intact instead of splitting on
        // the space, so preflight checks the real binary.
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
}
