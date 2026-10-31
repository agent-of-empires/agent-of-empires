//! `agent-of-empires add` command implementation

use crate::daemon::{
    CliCreationOptions, CreateSessionBody, CreationTrustRequest, DaemonClient, DaemonClientError,
    RepoBaseInput, RuntimeHealth, SessionResponse, RUNTIME_PROTOCOL_VERSION,
};
use anyhow::{bail, Context, Result};
use clap::Args;
use std::future::Future;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::pin::Pin;

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

    /// Session title (defaults to a civilization name, or the worktree branch)
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

#[tracing::instrument(target = "cli.add", skip_all, fields(profile = %profile))]
pub async fn run(profile: &str, args: AddArgs) -> Result<()> {
    use crate::acp::client::{require_daemon, HttpClient};
    if args.interactive && !std::io::stdin().is_terminal() {
        bail!("--interactive requires a terminal; pass --title for non-interactive naming");
    }
    if args
        .tool
        .as_deref()
        .is_some_and(|tool| tool.trim().is_empty())
    {
        bail!("--tool requires a non-empty agent name");
    }
    if args.scratch && args.path.is_some() {
        bail!("Cannot specify a project path with --scratch");
    }
    let sandbox = args.sandbox || args.sandbox_image.is_some();
    if !args.hook_env.is_empty() && !sandbox {
        bail!("--hook-env requires --sandbox");
    }
    let branch = args
        .worktree_branch
        .as_deref()
        .map(str::trim)
        .filter(|b| !b.is_empty());
    if (!args.extra_repos.is_empty()
        || !args.projects.is_empty()
        || !args.repo_bases.is_empty()
        || args.generated_name)
        && branch.is_none()
    {
        bail!("--repo/--project/--repo-base/--generated-name requires --worktree with a non-empty branch name");
    }
    if args.generated_name && (!args.extra_repos.is_empty() || !args.projects.is_empty()) {
        bail!("--generated-name does not apply to multi-repo workspaces");
    }
    let endpoint = require_daemon().await?;
    let local = endpoint.unix_path().is_some();
    let http = HttpClient::new(endpoint)?;
    let profile = super::send::resolve_profile(&http, profile).await?;
    let client = http.daemon_client().await?;
    let info = client.runtime_info().await?;
    anyhow::ensure!(
        info.protocol_version == RUNTIME_PROTOCOL_VERSION && info.health == RuntimeHealth::Healthy,
        "Daemon runtime unavailable or incompatible; run `aoe serve --restart`"
    );
    anyhow::ensure!(
        !info.read_only && !info.cityhall_mode,
        "Daemon does not permit CLI creation"
    );
    anyhow::ensure!(
        info.cli_creation,
        "Daemon does not support CLI creation; run `aoe serve --restart`"
    );
    let epoch = info.epoch.clone();
    let rows = client
        .list_sessions(Some(crate::session::SessionScope::All))
        .await?
        .sessions;
    let mut path = if args.scratch || args.fork_from.is_some() {
        String::new()
    } else {
        host_path(args.path.as_deref(), local)?
    };
    if let Some(source) = args.fork_from.as_deref() {
        path = select_source(&rows, &profile, source)?.project_path.clone();
    }
    let mut title = args.title.as_deref().map(str::trim).map(str::to_owned);
    let review = client
        .review_creation_trust(
            &CreationTrustRequest {
                path: path.clone(),
                profile: Some(profile.clone()),
                scratch: args.scratch,
                generated_worktree_branch: (args.generated_name && args.interactive)
                    .then(|| branch.unwrap_or_default().to_owned()),
                create_new_branch: args.create_branch,
            },
            &epoch,
        )
        .await?;
    if args.interactive && title.is_none() {
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        let default = if args.generated_name {
            review
                .suggested_title
                .clone()
                .context("Daemon omitted generated worktree title")?
        } else {
            branch
                .map(str::to_owned)
                .unwrap_or_else(|| crate::session::civilizations::generate_random_title(&titles))
        };
        title = Some(prompt_title(&default)?);
    }
    let trust = if !review.hooks_need_trust && !review.mcp_need_trust || args.trust_hooks {
        true
    } else if std::io::stdin().is_terminal() {
        for group in crate::session::config::repo_config::hook_display_groups(
            &review.merged_hooks,
            &review.repo_hooks,
            true,
        ) {
            eprintln!("{}:{}", group.name, group.source_label());
            for command in group.commands {
                eprintln!("  {command}");
            }
        }
        for server in &review.mcp_summaries {
            eprintln!("  {server}");
        }
        prompt_trust()?
    } else {
        false
    };
    let key = uuid::Uuid::new_v4().to_string();
    let body = CreateSessionBody {
        title,
        title_typed: args.title.is_some() || args.interactive,
        size: crate::terminal::get_size().and_then(|(cols, rows)| {
            Some(crate::daemon::TerminalSize {
                cols: std::num::NonZeroU16::new(cols)?,
                rows: std::num::NonZeroU16::new(rows)?,
            })
        }),
        path,
        tool: args.tool.unwrap_or_default(),
        group: args.group.as_deref().unwrap_or_default().trim().to_owned(),
        yolo_mode: args.yolo,
        worktree_enabled: branch.is_some(),
        worktree_branch: branch.map(str::to_owned),
        create_new_branch: args.create_branch,
        base_branch: args.base_branch,
        sandbox,
        sandbox_image: args.sandbox_image,
        extra_args: args.extra_args.unwrap_or_default(),
        extra_env: Vec::new(),
        hook_env: explicit_hook_env(&args.hook_env),
        extra_repo_paths: args
            .extra_repos
            .iter()
            .map(|path| host_path(Some(path), local))
            .collect::<Result<_>>()?,
        repo_bases: args
            .repo_bases
            .into_iter()
            .map(|(repo, base_branch)| {
                let raw = std::path::Path::new(&repo);
                let repo = if args.extra_repos.iter().any(|path| path == raw)
                    || args.path.as_deref() == Some(raw)
                {
                    host_path(Some(raw), local)?
                } else {
                    repo
                };
                Ok(RepoBaseInput { repo, base_branch })
            })
            .collect::<Result<_>>()?,
        command_override: args.cmd_override.unwrap_or_default(),
        custom_instruction: None,
        profile: Some(profile.clone()),
        view: if args.structured_view || args.agent.is_some() {
            crate::session::View::Structured
        } else {
            crate::session::View::Terminal
        },
        agent_name: args.agent,
        agent_model: args.model,
        agent_effort: args.effort,
        scratch: args.scratch,
        trust_hooks: Some(trust),
        trust_review: Some(review.fingerprint),
        import_acp_session_id: None,
        fork_from: None,
        fork_session_id: None,
        callback_url: None,
        idempotency_key: Some(key.clone()),
        retry_origin: None,
        cli: Some(CliCreationOptions {
            parent: args.parent,
            projects: args.projects,
            command: args.command,
            fork_source: args.fork_from,
            generated_name: args.generated_name,
            no_submodules: args.no_submodules,
            worktree_path_id: review.worktree_path_id,
        }),
    };
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    let row = create_and_observe(&client, &body, &epoch, &key, signal.as_mut()).await?;
    for warning in &row.warnings {
        eprintln!("Warning: {warning}");
    }
    println!("✓ Added session: {}", row.title);
    println!("  Profile: {}", row.profile);
    println!("  Path:    {}", row.project_path);
    println!("  Group:   {}", row.group_path);
    println!("  ID:      {}", row.id);
    if let Some(command) = body
        .cli
        .as_ref()
        .and_then(|options| options.command.as_deref())
    {
        println!("  Cmd:     {command}");
    }
    if let Some(parent) = &row.parent_session_id {
        println!("  Parent:  {parent}");
    }
    if row.is_sandboxed {
        println!("  Sandbox: enabled");
    }
    if row.scratch {
        println!("  Scratch:  yes");
    }
    if row.yolo_mode {
        println!("  YOLO:    enabled");
    }
    if !row.workspace_repos.is_empty() {
        println!("  Workspace: {} repos", row.workspace_repos.len());
        for repo in &row.workspace_repos {
            println!("    - {} ({})", repo.name, repo.worktree_path);
        }
    }
    let launch = if args.launch && row.view == crate::session::View::Terminal {
        let request = crate::daemon::StartSessionBody {
            size: crate::terminal::get_size().and_then(|(cols, rows)| {
                Some(crate::daemon::TerminalSize {
                    cols: std::num::NonZeroU16::new(cols)?,
                    rows: std::num::NonZeroU16::new(rows)?,
                })
            }),
        };
        Some(await_launch(
            async { Ok(client.ensure_agent(&row.id, &request, &epoch).await?) },
            signal.as_mut(),
        ).await.with_context(|| format!(
            "Session {} was created, but launch was not confirmed; inspect it before retrying", row.id
        ))?)
    } else {
        None
    };
    if args.launch && row.view == crate::session::View::Terminal {
        if local && std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
            let mut connection =
                crate::daemon::RuntimeConnection::connect(&require_daemon().await?, Some(&profile))
                    .await?;
            anyhow::ensure!(
                connection.native_interaction_allowed() && connection.info().epoch == epoch,
                "Daemon changed before attachment; session was created, attach explicitly"
            );
            let receipt = launch.context("Daemon launch receipt unavailable")?;
            tokio::time::timeout(std::time::Duration::from_secs(15), async {
                while connection.snapshot().cursor.revision < receipt.cursor.revision {
                    connection.next_snapshot().await?;
                }
                Ok::<_, crate::daemon::RuntimeConnectionError>(())
            })
            .await??;
            anyhow::ensure!(
                connection.native_interaction_allowed()
                    && connection.snapshot().cursor.epoch == receipt.cursor.epoch
                    && connection
                        .snapshot()
                        .contents
                        .sessions
                        .iter()
                        .any(|current| current.id == row.id
                            && current.lifecycle_generation
                                == receipt.outcome.lifecycle_generation
                            && current.agent_pane.tmux_session.as_deref()
                                == Some(receipt.outcome.tmux_session.as_str())),
                "Session changed before attachment; attach explicitly"
            );
            crate::tmux::Session::from_name(&receipt.outcome.tmux_session).attach()?;
        } else {
            println!("(session started without local attachment; use the Daemon host or dashboard to view it)");
        }
    } else if row.view == crate::session::View::Structured {
        println!("Structured session lifecycle is managed by the Daemon.");
    } else {
        println!(
            "Start with: aoe -p {} session start {}",
            shell_words::quote(&profile),
            shell_words::quote(&row.id)
        );
    }
    Ok(())
}

fn host_path(path: Option<&std::path::Path>, local: bool) -> Result<String> {
    if !local {
        let path = path
            .and_then(std::path::Path::to_str)
            .context("A remote Daemon requires an explicit absolute host path (or --scratch)")?;
        anyhow::ensure!(
            path.starts_with('/'),
            "Remote project and repository paths must be absolute paths on the Daemon host"
        );
        return Ok(path.to_owned());
    }
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => std::env::current_dir()?,
    };
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("Project path must be UTF-8"))
}

fn select_source<'a>(
    rows: &'a [SessionResponse],
    profile: &str,
    selector: &str,
) -> Result<&'a SessionResponse> {
    let rows: Vec<_> = rows.iter().filter(|row| row.profile == profile).collect();
    Ok(*super::resolve_session(selector, &rows)?)
}

fn prompt_title(default: &str) -> Result<String> {
    use std::io::Write;
    eprint!("Session name [{default}]: ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(if input.trim().is_empty() {
        default.to_owned()
    } else {
        input.trim().to_owned()
    })
}

fn prompt_trust() -> Result<bool> {
    use std::io::Write;
    eprint!("Trust this repo (hooks and project MCP shown above)? [y/N] ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(input.trim().eq_ignore_ascii_case("y"))
}

fn explicit_hook_env(entries: &[String]) -> Vec<String> {
    for warning in crate::session::validate_env_entries(entries) {
        eprintln!(
            "warning: {}",
            warning.replace("empty in the container", "skipped by the host hook")
        );
    }
    entries
        .iter()
        .filter_map(|entry| {
            let (key, value) = match entry.split_once('=') {
                Some((key, value)) => (key, crate::session::environment::resolve_env_value(value)),
                None => (entry.as_str(), std::env::var(entry).ok()),
            };
            value.map(|value| {
                let escape = if value.starts_with('$') { "$" } else { "" };
                format!("{key}={escape}{value}")
            })
        })
        .collect()
}

async fn await_launch<T>(
    launch: impl Future<Output = Result<T>>,
    signal: Pin<&mut impl Future<Output = std::io::Result<()>>>,
) -> Result<T> {
    // A queued interrupt must prevent submitting the next lifecycle mutation.
    tokio::select! {
        biased;
        result = signal => {
            result.context("Failed to listen for launch interruption")?;
            bail!("Launch interrupted");
        }
        result = launch => result,
    }
}

async fn create_and_observe(
    client: &DaemonClient,
    body: &CreateSessionBody,
    epoch: &str,
    key: &str,
    mut signal: Pin<&mut impl Future<Output = std::io::Result<()>>>,
) -> Result<SessionResponse> {
    let create = client.create_session(body, epoch);
    tokio::pin!(create);
    let (state, changes) = tokio::sync::watch::channel((false, false));
    let observation = observe_creation(client, body, epoch, key, changes);
    tokio::pin!(observation);
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(30 * 60));
    tokio::pin!(deadline);
    let mut pending = true;
    let mut cancel = false;
    loop {
        tokio::select! {
            result = &mut create, if pending => {
                pending = false;
                match result {
                    Ok(receipt) if !cancel => return Ok(receipt.outcome),
                    Ok(_) => bail!("Creation finished before cancellation; inspect request {key} before taking further action"),
                    Err(error) if definitive(&error) => return Err(error.into()),
                    Err(error) => eprintln!("Creation reply unavailable ({error}); reconciling request {key}, never resubmitting"),
                }
                state.send_replace((cancel, true));
                deadline.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(30));
            }
            result = &mut observation => return result,
            result = &mut signal, if !cancel => {
                result.context("Failed to listen for cancellation")?;
                cancel = true;
                state.send_replace((true, !pending));
                eprintln!("Cancelling creation {key}; waiting for the Daemon");
            }
            _ = &mut deadline => bail!("Creation outcome unknown for request {key}; check the Daemon session list before creating again"),
        }
    }
}

async fn observe_creation(
    client: &DaemonClient,
    body: &CreateSessionBody,
    epoch: &str,
    key: &str,
    changes: tokio::sync::watch::Receiver<(bool, bool)>,
) -> Result<SessionResponse> {
    let mut poll = tokio::time::interval(std::time::Duration::from_millis(200));
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cancel_sent = false;
    let mut previous = String::new();
    let mut previous_output = Vec::<String>::new();
    loop {
        poll.tick().await;
        if let Ok(progress) = client.creation_progress(key).await {
            let message = progress.hook.as_deref().unwrap_or(&progress.stage);
            if message != previous {
                eprintln!("{message}");
                previous = message.to_owned();
            }
            let overlap = (0..=previous_output.len().min(progress.output.len()))
                .rev()
                .find(|&length| {
                    previous_output[previous_output.len() - length..] == progress.output[..length]
                })
                .unwrap_or(0);
            for line in &progress.output[overlap..] {
                eprintln!("{line}");
            }
            previous_output = progress.output;
        }
        let (cancel, reconcile) = *changes.borrow();
        if !cancel && !reconcile {
            continue;
        }
        let snapshot = client.runtime_snapshot(epoch).await
            .with_context(|| format!("Creation outcome unknown for request {key}; do not create again without checking the Daemon"))?;
        anyhow::ensure!(
            snapshot.contents.health == RuntimeHealth::Healthy,
            "Creation outcome unknown for request {key}; Daemon state is not healthy"
        );
        if let Some(row) = snapshot.contents.sessions.into_iter().find(|row| {
            row.idempotency_key.as_deref() == Some(key)
                && Some(row.profile.as_str()) == body.profile.as_deref()
        }) {
            if row.status.eq_ignore_ascii_case("creating") {
                if cancel && !cancel_sent {
                    client.cancel_creation(&row.id, epoch).await
                        .context("Creation cancellation was not confirmed; inspect the Daemon before taking further action")?;
                    cancel_sent = true;
                }
            } else if creation_is_final(&row) {
                if cancel {
                    bail!(
                        "Creation settled before cancellation; inspect session {}",
                        row.id
                    );
                }
                return Ok(row);
            }
        }
    }
}

fn creation_is_final(row: &SessionResponse) -> bool {
    row.lifecycle_reservation.is_none()
        && match row.view {
            crate::session::View::Terminal => row.status.eq_ignore_ascii_case("stopped"),
            crate::session::View::Structured => ["running", "waiting", "idle", "stopped"]
                .iter()
                .any(|status| row.status.eq_ignore_ascii_case(status)),
        }
}

fn definitive(error: &DaemonClientError) -> bool {
    matches!(error, DaemonClientError::Status { status, code, .. }
        if status.is_client_error() && *status != reqwest::StatusCode::REQUEST_TIMEOUT
            && *code != Some(crate::daemon::ApiErrorCode::CreateOutcomeUnknown))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_interruption_does_not_submit_launch() {
        let submitted = std::cell::Cell::new(false);
        let (interrupt, received) = tokio::sync::oneshot::channel();
        let signal = async move {
            received.await.unwrap();
            Ok(())
        };
        tokio::pin!(signal);
        assert!(futures_util::poll!(signal.as_mut()).is_pending());
        interrupt.send(()).unwrap();
        let result = await_launch(
            async {
                submitted.set(true);
                Ok(())
            },
            signal.as_mut(),
        )
        .await;
        assert!(result.is_err());
        assert!(!submitted.get());
    }

    #[test]
    fn reconciliation_excludes_launch_and_rollback_in_progress() {
        use crate::session::{Instance, LifecycleOperation, View};
        let mut instance = Instance::new("pending", "/tmp");
        instance
            .try_acquire_lifecycle_reservation(
                LifecycleOperation::Launch,
                Instance::LIFECYCLE_RESERVATION_TTL,
                chrono::Utc::now(),
            )
            .unwrap();
        for view in [View::Terminal, View::Structured] {
            for status in [
                "creating", "starting", "deleting", "error", "unknown", "", "stopped", "running",
                "waiting", "idle",
            ] {
                for reserved in [false, true] {
                    let row: SessionResponse = serde_json::from_value(serde_json::json!({
                        "id": "pending", "view": view, "status": status,
                        "lifecycle_reservation": if reserved { instance.lifecycle_reservation.clone() } else { None },
                    })).unwrap();
                    let expected = !reserved
                        && (status == "stopped"
                            || view == View::Structured
                                && matches!(status, "running" | "waiting" | "idle"));
                    assert_eq!(
                        creation_is_final(&row),
                        expected,
                        "{view:?} {status} {reserved}"
                    );
                }
            }
        }
    }

    #[test]
    fn an_uncertain_create_is_not_reported_as_rejected() {
        use crate::daemon::ApiErrorCode;
        use reqwest::StatusCode;
        for (status, code, rejected) in [
            (StatusCode::BAD_REQUEST, None, true),
            (StatusCode::REQUEST_TIMEOUT, None, false),
            (StatusCode::INTERNAL_SERVER_ERROR, None, false),
            (
                StatusCode::CONFLICT,
                Some(ApiErrorCode::CreateOutcomeUnknown),
                false,
            ),
            (
                StatusCode::CONFLICT,
                Some(ApiErrorCode::CreationCancelled),
                true,
            ),
        ] {
            assert_eq!(
                definitive(&DaemonClientError::Status {
                    status,
                    code,
                    body: String::new(),
                    truncated: false,
                }),
                rejected
            );
        }
        assert!(!definitive(&DaemonClientError::Timeout));
        assert!(!definitive(&DaemonClientError::Transport));
    }

    #[test]
    fn explicit_hook_values_survive_daemon_dollar_expansion() {
        let entries = [
            "EMPTY=",
            "DOLLAR=$$literal",
            "DOUBLE=$$$literal",
            "TEXT=literal",
        ];
        let encoded = explicit_hook_env(&entries.map(str::to_owned));
        let decoded: Vec<_> = encoded
            .iter()
            .map(|entry| {
                let (key, value) = entry.split_once('=').unwrap();
                (
                    key,
                    crate::session::environment::resolve_env_value(value).unwrap(),
                )
            })
            .collect();
        assert_eq!(
            decoded,
            [
                ("EMPTY", String::new()),
                ("DOLLAR", "$literal".to_owned()),
                ("DOUBLE", "$$literal".to_owned()),
                ("TEXT", "literal".to_owned()),
            ]
        );
    }
}
