//! Daemon-side selection for CLI creation requests.

use anyhow::{bail, Context, Result};

use crate::daemon::CreateSessionBody;
use crate::session::Instance;

pub(crate) fn resolve_cli_session<'a>(
    rows: &'a [Instance],
    profile: &str,
    selector: &str,
) -> Result<&'a Instance> {
    let rows: Vec<_> = rows
        .iter()
        .filter(|row| row.effective_profile() == profile)
        .collect();
    Ok(*crate::cli::resolve_session(selector, &rows)?)
}

pub(super) async fn resolve_cli_creation(
    state: &super::AppState,
    mut body: CreateSessionBody,
    default_profile: &str,
) -> Result<(CreateSessionBody, Vec<String>)> {
    let Some(mut options) = body.cli.take() else {
        return Ok((body, Vec::new()));
    };
    anyhow::ensure!(
        !state.cityhall_mode,
        "CityHall does not permit CLI creation"
    );
    body.agent_effort = body
        .agent_effort
        .take()
        .map(|effort| effort.trim().to_owned());
    let profile = body.profile.as_deref().unwrap_or(default_profile);
    let mut inherited_tool = None;
    if let Some(selector) = options.fork_source.as_deref() {
        anyhow::ensure!(
            body.view == crate::session::View::Terminal && body.agent_name.is_none(),
            "--fork-from cannot be combined with --structured-view or --agent"
        );
        anyhow::ensure!(
            !body.scratch
                && !body.worktree_enabled
                && body.worktree_branch.is_none()
                && !body.create_new_branch
                && !body.sandbox
                && body.command_override.is_empty(),
            "--fork-from cannot use scratch, worktrees, sandbox or --cmd-override"
        );
        let rows = state.instances.read().await;
        let source = resolve_cli_session(&rows, profile, selector)?;
        body.path = source.project_path.clone();
        body.fork_session_id = Some(source.id.clone());
        inherited_tool = Some(source.tool.clone());
    }
    if let Some(selector) = options.parent.as_deref() {
        let rows = state.instances.read().await;
        let parent = resolve_cli_session(&rows, profile, selector)?;
        anyhow::ensure!(
            parent.parent_session_id.is_none(),
            "Cannot create a sub-session of a sub-session"
        );
        body.group = parent.group_path.clone();
        options.parent = Some(parent.id.clone());
    }
    let default_profile = if body.profile.is_none() {
        default_profile.to_owned()
    } else {
        String::new()
    };
    tokio::task::spawn_blocking(move || {
        resolve_host_creation(body, options, inherited_tool, &default_profile)
    })
    .await
    .map_err(|error| {
        anyhow::Error::new(crate::server::session_spawn::SessionBuildPanicked(
            error.to_string(),
        ))
    })?
}

fn resolve_host_creation(
    mut body: CreateSessionBody,
    options: crate::daemon::CliCreationOptions,
    inherited_tool: Option<String>,
    default_profile: &str,
) -> Result<(CreateSessionBody, Vec<String>)> {
    let profile = body.profile.as_deref().unwrap_or(default_profile);
    let mut warnings = Vec::new();
    let mut original_repos = if body.create_new_branch && !body.repo_bases.is_empty() {
        vec![std::path::PathBuf::from(&body.path)]
    } else {
        Vec::new()
    };
    if !body.scratch {
        let path = std::path::Path::new(&body.path)
            .canonicalize()
            .with_context(|| format!("Failed to resolve Daemon host path: {}", body.path))?;
        anyhow::ensure!(path.is_dir(), "Path is not a directory: {}", path.display());
        crate::session::builder::ensure_not_purging(&path)?;
        body.path = path.to_string_lossy().into_owned();
    }
    let config = if body.scratch {
        crate::session::config::profile_config::resolve_config(profile)?
    } else {
        crate::session::config::repo_config::resolve_config_with_repo(
            profile,
            std::path::Path::new(&body.path),
        )?
    };
    if !options.projects.is_empty() {
        body.extra_repo_paths.extend(
            crate::session::projects::resolve_names(profile, &options.projects)?
                .into_iter()
                .map(|project| project.path),
        );
    }
    if !original_repos.is_empty() {
        original_repos.extend(body.extra_repo_paths.iter().map(std::path::PathBuf::from));
        let mut pairs: Vec<_> = std::mem::take(&mut body.repo_bases)
            .into_iter()
            .map(|pair| (pair.repo, pair.base_branch))
            .collect();
        let mono_repo = body.extra_repo_paths.is_empty();
        if mono_repo {
            let main = crate::git::GitWorktree::find_main_repo(std::path::Path::new(&body.path))?
                .canonicalize()
                .context("Failed to resolve the principal repository on the Daemon host")?;
            let alias = &original_repos[0];
            for (selector, _) in &mut pairs {
                let selected = selector.trim();
                if alias.as_os_str() == selected
                    || alias
                        .file_name()
                        .is_some_and(|name| name == std::ffi::OsStr::new(selected))
                {
                    *selector = main.to_string_lossy().into_owned();
                }
            }
            original_repos[0] = main;
        }
        body.repo_bases =
            crate::session::builder::resolve_repo_base_selectors(&original_repos, &pairs)?
                .into_iter()
                .map(|(path, base_branch)| {
                    let path = if mono_repo {
                        path
                    } else {
                        path.canonicalize().context(
                            "Failed to resolve a repository base selector on the Daemon host",
                        )?
                    };
                    Ok(crate::daemon::RepoBaseInput {
                        repo: path.to_string_lossy().into_owned(),
                        base_branch,
                    })
                })
                .collect::<Result<_>>()?;
    }
    for path in &mut body.extra_repo_paths {
        *path = std::path::Path::new(path)
            .canonicalize()
            .context("Failed to resolve an additional repository on the Daemon host")?
            .to_string_lossy()
            .into_owned();
    }
    anyhow::ensure!(
        body.extra_repo_paths.is_empty()
            || body
                .worktree_branch
                .as_deref()
                .is_some_and(|b| !b.trim().is_empty()),
        "--repo/--project requires --worktree to specify a branch"
    );
    anyhow::ensure!(
        !options.generated_name || body.extra_repo_paths.is_empty(),
        "--generated-name does not apply to multi-repo workspaces"
    );
    let explicit_tool = !body.tool.trim().is_empty() || options.command.is_some();
    if let Some(command) = options.command.as_deref() {
        body.tool = crate::agents::resolve_tool_name(command)
            .with_context(|| format!("Unknown tool in command: {command}"))?
            .to_owned();
        if command.trim().contains(' ')
            && body.command_override.is_empty()
            && config.session.resolve_tool_command(&body.tool).is_empty()
        {
            body.command_override = command.to_owned();
        }
    } else if !body.tool.trim().is_empty() {
        let name = body.tool.trim();
        if let Some(command) = config.session.custom_agents.get(name) {
            anyhow::ensure!(
                !command.trim().is_empty(),
                "custom agent '{name}' has an empty configured command"
            );
            anyhow::ensure!(
                body.command_override.is_empty(),
                "--cmd-override cannot be used with configured custom agent --tool selections"
            );
            if let Some(detect_as) = config
                .session
                .agent_detect_as
                .get(name)
                .filter(|name| !name.trim().is_empty())
            {
                anyhow::ensure!(
                    crate::agents::get_agent(detect_as).is_some(),
                    "Unknown agent_detect_as for custom agent '{name}'"
                );
            }
            body.tool = name.to_owned();
        } else {
            body.tool = crate::agents::resolve_tool_name(name)
                .with_context(|| format!("Unknown tool: {name}"))?
                .to_owned();
        }
    } else {
        body.tool = inherited_tool
            .or_else(|| {
                config.session.default_tool.as_deref().and_then(|name| {
                    if config.session.custom_agents.contains_key(name) {
                        Some(name.to_owned())
                    } else {
                        crate::agents::resolve_tool_name(name).map(str::to_owned)
                    }
                })
            })
            .or_else(|| {
                crate::tmux::AvailableTools::detect()
                    .available_list()
                    .first()
                    .cloned()
            })
            .unwrap_or_else(|| "claude".to_owned());
        if !config.session.custom_agents.contains_key(&body.tool) {
            if let Some(name) = crate::agents::resolve_tool_name(&body.tool) {
                body.tool = name.to_owned();
            }
        }
    }
    if explicit_tool {
        let configured = config.session.resolve_tool_command(&body.tool);
        if options.command.is_some() && !configured.is_empty() {
            let binary = shell_words::split(&configured)?
                .into_iter()
                .next()
                .context("Configured agent command is empty")?;
            anyhow::ensure!(
                crate::tmux::is_binary_on_path(&binary),
                "Configured agent binary is unavailable on the Daemon host: {binary}"
            );
        } else if let Some(agent) = crate::agents::get_agent(&body.tool)
            .filter(|_| !config.session.custom_agents.contains_key(&body.tool))
        {
            anyhow::ensure!(
                crate::tmux::is_agent_available(agent),
                "Agent '{}' is not installed on the Daemon host. Install with: {}",
                agent.binary,
                agent.install_hint
            );
        }
    }
    body.yolo_mode |= config.session.yolo_mode_default;
    body.sandbox |= config.sandbox.enabled_by_default
        && crate::containers::get_container_runtime().is_available();
    if body.sandbox {
        let explicit = body
            .sandbox_image
            .take()
            .map(|mut image| {
                image.truncate(image.trim_end().len());
                let leading = image.len() - image.trim_start().len();
                if leading != 0 {
                    image.drain(..leading);
                }
                image
            })
            .filter(|image| !image.is_empty());
        body.sandbox_image = Some(match explicit {
            Some(image) => image,
            None => {
                let configured = config.sandbox.default_image.trim();
                if configured.is_empty() {
                    crate::containers::get_container_runtime()
                        .default_sandbox_image()
                        .to_owned()
                } else {
                    configured.to_owned()
                }
            }
        });
    }
    if body.view == crate::session::View::Structured {
        use crate::session::builder::structured::{
            preparation_hooks_apply, validate_structured_choice, StructuredChoice,
        };
        let profile_hooks = crate::session::config::profile_config::resolve_config(profile)?.hooks;
        let choice = StructuredChoice {
            tool: &body.tool,
            agent_name: body.agent_name.as_deref(),
            command_override: &body.command_override,
            sandboxed: body.sandbox,
            project_path: (!(body.scratch
                || body.worktree_enabled
                || !body.extra_repo_paths.is_empty()))
            .then(|| std::path::Path::new(&body.path)),
            preparation_hooks: &|| {
                preparation_hooks_apply(
                    &profile_hooks,
                    std::path::Path::new(&body.path),
                    body.scratch,
                    body.trust_hooks,
                )
            },
        };
        if let Err(error) = validate_structured_choice(&choice, &config) {
            if matches!(
                error,
                crate::session::builder::structured::StructuredChoiceError::NotCapable(_)
            ) || body.agent_name.is_some()
                || body.agent_effort.is_some()
            {
                bail!("{error}");
            }
            warnings.push(format!("{error}; using terminal view"));
            body.view = crate::session::View::Terminal;
        }
    }
    if body
        .agent_effort
        .as_deref()
        .is_some_and(|effort| effort.trim().is_empty())
    {
        bail!("--effort cannot be empty");
    }
    if body.agent_effort.is_some() && body.view != crate::session::View::Structured {
        bail!("--effort needs the structured view; pass --agent or --structured-view");
    }
    if let Some(effort) = body.agent_effort.as_deref() {
        let agent = body
            .agent_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&body.tool);
        let choices = crate::acp::option_catalog::thought_level_choices(agent);
        if choices.is_empty() {
            warnings.push(format!("no known effort choices for ACP agent `{agent}`; passing `{effort}` through unchecked"));
        } else {
            anyhow::ensure!(
                choices.iter().any(|choice| choice == effort),
                "effort `{effort}` is not offered by ACP agent `{agent}`. Valid values: {}",
                choices.join(", ")
            );
        }
    }
    body.cli = Some(options);
    Ok((body, warnings))
}
