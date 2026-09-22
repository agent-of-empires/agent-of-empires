//! Agent of Empires: terminal session manager for AI coding agents.

use agent_of_empires::cli::{self, Cli, Commands};
use agent_of_empires::logging::{self, LogConfig, ProcessContext, SubscriberTarget};
use agent_of_empires::migrations;
use agent_of_empires::tui;
use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
use clap_complete::generate;

fn is_serve_command(cli: &Cli) -> bool {
    matches!(cli.command, Some(Commands::Serve(_)))
}

/// Runs before the tokio worker pool exists, so `set_var` is sound.
fn seed_cityhall_env(cli: &Cli) {
    if let Some(Commands::Serve(args)) = &cli.command {
        if args.cityhall {
            // SAFETY: single-threaded here, same invariant as the
            // AOE_DAEMON_URL seed above (no worker threads spawned yet).
            unsafe {
                std::env::set_var("AOE_CITYHALL_MODE", "1");
            }
        }
    }
}

/// The child's stdio is redirected to the log file, so tracing writes there too.
fn is_serve_daemon_child(cli: &Cli) -> bool {
    matches!(cli.command, Some(Commands::Serve(ref args)) if args.daemon_child)
}

/// With the `aoe.web` plugin disabled, starting `aoe serve` is an unknown subcommand;
/// the lifecycle verbs stay usable.
fn serve_unavailable_error(cli: &Cli) -> Option<clap::Error> {
    cli::graft::serve_start_blocked(cli, cli::graft::web_disabled()).then(|| {
        Cli::command().error(
            clap::error::ErrorKind::InvalidSubcommand,
            "unrecognized subcommand 'serve'",
        )
    })
}

#[tokio::main]
async fn main() -> Result<()> {
    // Hidden helper for the VT live preview, handled before clap so it stays off the CLI surface.
    {
        let mut a = std::env::args();
        let _ = a.next();
        if a.next().as_deref() == Some("__vt-pipe") {
            let sock = a.next().unwrap_or_default();
            return agent_of_empires::tui::run_vt_pipe(&sock).map_err(Into::into);
        }
    }

    // Hidden smart-rename helper, handled before clap so it stays off the CLI surface.
    {
        let mut a = std::env::args();
        let _ = a.next();
        if a.next().as_deref() == Some("__smart-rename") {
            let mut next = a.next();
            let force = next.as_deref() == Some("--force");
            if force {
                next = a.next();
            }
            let profile = next.unwrap_or_default();
            let session_id = a.next().unwrap_or_default();
            let _ = agent_of_empires::session::smart_rename::run_smart_rename_now(
                &profile,
                &session_id,
                force,
            )
            .await;
            return Ok(());
        }
    }

    // Only a parse failure loads the plugin registry to graft plugin commands.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(_) => {
            let matches = cli::graft::augmented_command().get_matches();
            match Cli::from_arg_matches(&matches) {
                Ok(cli) => cli,
                Err(_) => return cli::graft::dispatch_plugin_command(&matches),
            }
        }
    };

    if let Some(err) = serve_unavailable_error(&cli) {
        err.exit();
    }

    if let Some(url) = &cli.daemon_url {
        // SAFETY: single-threaded at this point — we haven't entered
        // the tokio runtime's worker pool yet (the runtime is owned by
        // the `#[tokio::main]` wrapper that called us, and clap's
        // parsing was synchronous).
        unsafe {
            std::env::set_var("AOE_DAEMON_URL", url);
        }
    }

    seed_cityhall_env(&cli);

    // Before anything calls `get_app_dir()`, which would create the dev dir.
    let debug_namespace_drift = agent_of_empires::session::debug_namespace_drift();

    let mut debug_log_warning: Option<String> = None;
    let env_cfg = LogConfig::from_env();
    let env_filter = env_cfg.filter_string();
    let is_serve = is_serve_command(&cli);
    let is_daemon_child = is_serve_daemon_child(&cli);
    let is_tui = cli.command.is_none();

    let ctx = if is_daemon_child {
        ProcessContext::ServeDaemonChild
    } else if is_serve {
        ProcessContext::ServeForeground
    } else if is_tui {
        ProcessContext::Tui
    } else {
        ProcessContext::OneShotCli
    };

    // One-shot CLI runs get no subscriber unless `AOE_LOG_LEVEL` is set.
    let should_init = matches!(
        ctx,
        ProcessContext::Tui | ProcessContext::ServeForeground | ProcessContext::ServeDaemonChild
    ) || env_filter.is_some();

    let (init, log_path_for_msg) = if should_init {
        let filter = env_filter
            .clone()
            .or_else(logging::load_persisted_filter)
            .unwrap_or_else(logging::serve_default_filter);

        match agent_of_empires::session::get_app_dir() {
            Ok(app_dir) => {
                let loaded_config = match agent_of_empires::session::load_config() {
                    Ok(opt) => opt,
                    Err(e) => {
                        eprintln!("warning: could not load config, using built-in defaults: {e}");
                        None
                    }
                };
                let log_cfg = loaded_config
                    .as_ref()
                    .map(|c| c.logging.clone())
                    .unwrap_or_default();
                let resolution = logging::resolve_sink(&log_cfg, &app_dir, ctx);
                let path_for_msg = match &resolution.target {
                    SubscriberTarget::File(p, _) => Some(p.clone()),
                    SubscriberTarget::Stdout => None,
                };
                let session_tee = if matches!(
                    ctx,
                    ProcessContext::ServeForeground | ProcessContext::ServeDaemonChild
                ) {
                    Some(agent_of_empires::acp::session_tee::SessionTeeLayer::new())
                } else {
                    None
                };
                let res = logging::init_subscriber_with_options(
                    resolution.target,
                    filter,
                    log_cfg.show_spans,
                    session_tee,
                );
                if let Some(w) = resolution.warning {
                    tracing::warn!(target: "log.runtime", "{}", w);
                }
                (res, path_for_msg)
            }
            Err(_) => (
                logging::InitResult {
                    controller: None,
                    warning: if env_filter.is_some() {
                        Some(
                            "Log level requested but app dir unavailable; file logging disabled."
                                .to_string(),
                        )
                    } else {
                        None
                    },
                },
                None,
            ),
        }
    } else {
        (
            logging::InitResult {
                controller: None,
                warning: None,
            },
            None,
        )
    };

    if let Some(c) = init.controller.clone() {
        logging::install_controller(c);
    }
    if let Some(msg) = init.warning {
        debug_log_warning = Some(msg);
    }
    if let (Some(_), Some(path), Some(lvl)) = (
        init.controller.as_ref(),
        log_path_for_msg.as_ref(),
        env_cfg.level,
    ) {
        tracing::info!(target: "log.runtime", "Debug logging at {} to {}", lvl.as_str(), path.display());
    }

    // `{e:#}` keeps the cause chain on one line for the line-oriented sink. The detached
    // daemon child skips `eprintln!` because its stderr already goes to the log.
    if let Err(e) = run(
        cli,
        is_daemon_child,
        should_init,
        debug_namespace_drift,
        debug_log_warning,
    )
    .await
    {
        tracing::error!(target: "log.runtime", "fatal: {e:#}");
        if !is_daemon_child {
            eprintln!("Error: {e:#}");
        }
        std::process::exit(1);
    }

    Ok(())
}

async fn run(
    cli: Cli,
    is_daemon_child: bool,
    should_init: bool,
    debug_namespace_drift: Option<(std::path::PathBuf, std::path::PathBuf)>,
    debug_log_warning: Option<String>,
) -> Result<()> {
    if cli.command.is_some() {
        if let Some((release, dev)) = debug_namespace_drift.as_ref() {
            eprintln!(
                "\n{}\n",
                agent_of_empires::session::format_debug_namespace_warning(release, dev),
            );
        }
    }

    // Skipped for the detached daemon child so `aoe serve --daemon` counts once.
    if !is_daemon_child {
        if let Some(name) = cli.command.as_ref().and_then(cli::command_name) {
            agent_of_empires::telemetry::track_cli_command(name).await;
        }
    }

    // No app data or migrations needed; these work in read-only environments such as Nix builds.
    match cli.command {
        Some(Commands::Completion { shell }) => {
            generate(shell, &mut Cli::command(), "aoe", &mut std::io::stdout());
            return Ok(());
        }
        Some(Commands::Init(args)) => return cli::init::run(args).await,
        Some(Commands::ExtractSessionId(args)) => return cli::extract_session_id::run(args).await,
        Some(Commands::Tmux { command }) => {
            use cli::tmux::TmuxCommands;
            return match command {
                TmuxCommands::Status(args) => cli::tmux::run_status(args),
            };
        }
        Some(Commands::Agents) => return cli::agents::run(),
        Some(Commands::Logs(args)) => return cli::logs::run(args).await,
        Some(Commands::LogLevel(args)) => return cli::log_level::run(args).await,
        Some(Commands::Sounds { command }) => return cli::sounds::run(command).await,
        Some(Commands::Theme { command }) => {
            use cli::theme::ThemeCommands;
            return match command {
                ThemeCommands::List => {
                    cli::theme::run_list();
                    Ok(())
                }
                ThemeCommands::Export { name, output } => {
                    cli::theme::run_export(&name, output.as_deref())
                }
                ThemeCommands::Dir => cli::theme::run_dir(),
            };
        }
        Some(Commands::Settings { command }) => return cli::settings::run(command),
        Some(Commands::Telemetry { command }) => return cli::telemetry::run(command),
        Some(Commands::Mcp { command }) => {
            let profile = cli.profile.clone().unwrap_or_default();
            return cli::mcp::run(&profile, command).await;
        }
        Some(Commands::Skill { command }) => return cli::skill::run(command),
        Some(Commands::Uninstall(args)) => return cli::uninstall::run(args).await,
        Some(Commands::Update(args)) => return cli::update::run(args).await,
        Some(Commands::Migrate) => return cli::migrate::run(),
        Some(Commands::Stop { .. }) => return cli::killall::stop_trap(),
        _ => {}
    }

    let profile_explicit = cli.profile.is_some();
    let profile = cli.profile.unwrap_or_default();

    if cli.command.is_some() {
        let reporter = cli
            .command
            .as_ref()
            .and_then(cli::command_name)
            .is_some()
            .then(cli::migrate::stderr_reporter);
        migrations::run_migrations_with(reporter)?;
        agent_of_empires::session::poller::configure_session_id_poller_max_threads(
            agent_of_empires::session::poller::configured_session_id_poller_max_threads(&profile),
        );
    }

    // Unknown keys are only reported here; parse failures only when no subscriber is up.
    // Hidden machine-spawned subcommands never print into a worker's redirected stderr.
    if cli.command.as_ref().and_then(cli::command_name).is_some() {
        let warning = if should_init {
            agent_of_empires::session::collect_startup_ignored_key_warnings(&profile)
        } else {
            agent_of_empires::session::collect_startup_config_warnings(&profile)
        };
        if let Some(w) = warning {
            eprintln!("{w}");
        }
    }

    let result = match cli.command {
        Some(Commands::Add(args)) => cli::add::run(&profile, *args).await,
        Some(Commands::List(args)) => cli::list::run(&profile, args).await,
        Some(Commands::Ps(args)) => cli::ps::run(&profile, profile_explicit, args).await,
        Some(Commands::Remove(args)) => cli::remove::run(&profile, args).await,
        Some(Commands::Send(args)) => cli::send::run(&profile, args).await,
        Some(Commands::Status(args)) => cli::status::run(&profile, args).await,
        Some(Commands::Killall(args)) => cli::killall::run(args).await,
        Some(Commands::Session { command }) => cli::session::run(&profile, command).await,
        Some(Commands::Group { command }) => cli::group::run(&profile, command).await,
        Some(Commands::Plugin { command }) => cli::plugin::run(command).await,
        Some(Commands::Profile { command }) => cli::profile::run(&profile, command).await,
        Some(Commands::Project { command }) => {
            cli::project::run(&profile, profile_explicit, command).await
        }
        Some(Commands::Worktree { command }) => cli::worktree::run(&profile, command).await,
        // Runs after migrations because `apply` writes the project registry.
        Some(Commands::Cityhall { command }) => cli::cityhall::run(command),
        Some(Commands::Serve(args)) => cli::serve::run(&profile, args).await,
        Some(Commands::Url(args)) => cli::url::run(args),
        Some(Commands::Sandbox { command }) => cli::sandbox::run(command),
        Some(Commands::Acp { command }) => cli::acp::run(command).await,
        Some(Commands::AcpRunner(args)) => agent_of_empires::process::runner::run(*args).await,
        None => {
            let drift_msg = debug_namespace_drift.as_ref().map(|(release, dev)| {
                agent_of_empires::session::format_debug_namespace_warning(release, dev)
            });
            let combined = match (debug_log_warning, drift_msg) {
                (Some(a), Some(b)) => Some(format!("{a}\n\n{b}")),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            tui::run(&profile, combined).await
        }
        _ => unreachable!(),
    };

    result
}
