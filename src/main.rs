//! Agent of Empires - Terminal session manager for AI coding agents

use agent_of_empires::cli::{self, Cli, Commands};
use agent_of_empires::logging::{self, LogConfig, ProcessContext, SubscriberTarget};
use agent_of_empires::migrations;
use agent_of_empires::tui;
use anyhow::Result;
use clap::{CommandFactory, Parser};
use clap_complete::generate;

/// Did the user invoke `aoe serve`? Feature-gated because `Commands::Serve`
/// only exists when the `serve` feature is on; in TUI-only builds we
/// always return false so the tracing-init branch below compiles.
#[cfg(feature = "serve")]
fn is_serve_command(cli: &Cli) -> bool {
    matches!(cli.command, Some(Commands::Serve(_)))
}

#[cfg(not(feature = "serve"))]
fn is_serve_command(_cli: &Cli) -> bool {
    false
}

/// Did the parent `aoe serve --daemon` spawn this process as the detached
/// child? Set by `start_daemon()` via the hidden `--daemon-child` flag.
/// Drives sink resolution: child's stdout/stderr are redirected to the
/// configured log file, so tracing must also write there (a Stdout sink
/// would land bytes in the same file via the OS redirect, but mixing two
/// writers on the same fd hurts ordering, and the configured-sink path
/// is what the TUI dialog and `aoe logs` tail).
#[cfg(feature = "serve")]
fn is_serve_daemon_child(cli: &Cli) -> bool {
    matches!(cli.command, Some(Commands::Serve(ref args)) if args.daemon_child)
}

#[cfg(not(feature = "serve"))]
fn is_serve_daemon_child(_cli: &Cli) -> bool {
    false
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // If the user passed --daemon-url, mirror the value into the env
    // var so the cockpit::client::discovery layer (used by both the
    // remote TUI home and the `aoe cockpit *` verbs) picks it up
    // through the same code path the env-only path uses. This avoids a
    // second "is the flag set?" check in every callsite.
    if let Some(url) = &cli.daemon_url {
        // SAFETY: single-threaded at this point — we haven't entered
        // the tokio runtime's worker pool yet (the runtime is owned by
        // the `#[tokio::main]` wrapper that called us, and clap's
        // parsing was synchronous).
        unsafe {
            std::env::set_var("AOE_DAEMON_URL", url);
        }
    }

    // Detect drift between release-build state and dev-build state BEFORE
    // anything below calls `get_app_dir()` (which would auto-create the dev
    // dir and silently flip the trigger condition for the rest of this
    // process). Compiled away in release builds.
    let debug_namespace_drift = agent_of_empires::session::debug_namespace_drift();

    // Lazy holder for the loaded config. Populated by the logging-init block
    // when it needs the `[logging]` section, and reused by the session-id
    // poller seed below the early-return command dispatch. Staying lazy here
    // means commands that don't need app data (`aoe completion`,
    // `aoe init`, `aoe agents`, `aoe uninstall`, `aoe update`, …) never
    // call `get_app_dir()` as a side effect. `config_load_attempted` lets
    // the seed block skip a redundant load (and a redundant error warning)
    // when the logging block already tried.
    let mut loaded_config: Option<agent_of_empires::session::Config> = None;
    let mut config_load_attempted = false;

    let mut debug_log_warning: Option<String> = None;
    // Subscriber installation. One resolver picks the sink based on
    // `ProcessContext` + `[logging]` config (see `logging::resolve_sink`).
    // Filter precedence: env (AOE_LOG_LEVEL / AGENT_OF_EMPIRES_DEBUG /
    // overlay vars) > `[logging]` config > info baseline. See
    // `docs/development/logging.md` for the sink and filter matrix.
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

    // One-shot CLI without an env override gets no subscriber: short-lived,
    // not worth the overhead. Opt in via `AOE_LOG_LEVEL=...`.
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
                loaded_config = match agent_of_empires::session::load_config() {
                    Ok(opt) => opt,
                    Err(e) => {
                        eprintln!("warning: could not load config, using built-in defaults: {e}");
                        None
                    }
                };
                config_load_attempted = true;
                let log_cfg = loaded_config
                    .as_ref()
                    .map(|c| c.logging.clone())
                    .unwrap_or_default();
                let resolution = logging::resolve_sink(&log_cfg, &app_dir, ctx);
                let path_for_msg = match &resolution.target {
                    SubscriberTarget::File(p, _) => Some(p.clone()),
                    SubscriberTarget::Stdout => None,
                };
                let res = logging::init_subscriber_with_options(
                    resolution.target,
                    filter,
                    log_cfg.show_spans,
                );
                if let Some(w) = resolution.warning {
                    // Emit through the subscriber that just came up.
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

    // CLI invocations get the dev-namespace drift warning on stderr right
    // away. TUI mode handles it via the existing startup-warning popup
    // pipeline below — we don't print here for TUI because ratatui's
    // alt-screen would clobber the message.
    if cli.command.is_some() {
        if let Some((release, dev)) = debug_namespace_drift.as_ref() {
            eprintln!(
                "\n{}\n",
                agent_of_empires::session::format_debug_namespace_warning(release, dev),
            );
        }
    }

    // Handle commands that don't need app data or migrations.
    // These work in read-only/sandboxed environments (e.g. Nix builds).
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
        #[cfg(feature = "serve")]
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
        Some(Commands::Telemetry { command }) => return cli::telemetry::run(command),
        Some(Commands::Uninstall(args)) => return cli::uninstall::run(args).await,
        Some(Commands::Update(args)) => return cli::update::run(args).await,
        _ => {}
    }

    let profile_explicit = cli.profile.is_some();
    let profile = cli.profile.unwrap_or_default();

    // Seed the session-id poller cap from persisted config. Reached only
    // for commands that may spawn sessions (early-return commands above
    // have already exited). Reuses the config loaded by the logging-init
    // block when available; otherwise loads now. Skips a redundant load
    // (and a redundant warning) when the logging block already attempted.
    let cap_config = if let Some(cfg) = loaded_config.take() {
        Some(cfg)
    } else if config_load_attempted {
        None
    } else {
        match agent_of_empires::session::load_config() {
            Ok(opt) => opt,
            Err(e) => {
                eprintln!(
                    "warning: could not load config to seed session-id poller cap, \
                     using built-in default of {}: {e}",
                    agent_of_empires::session::poller::DEFAULT_SESSION_ID_POLLER_MAX_THREADS,
                );
                None
            }
        }
    };
    if let Some(cfg) = cap_config {
        agent_of_empires::session::poller::set_session_id_poller_max_threads(
            cfg.session.session_id_poller_max_threads,
        );
    }

    // TUI mode handles migrations with a spinner; CLI runs them silently
    if cli.command.is_some() {
        migrations::run_migrations()?;
    }

    // Whether this invocation is a short-lived CLI command. The long-running
    // surfaces (TUI, serve) emit their own `process_start` with the correct
    // surface and run their own snapshot loop, so they're excluded here.
    #[cfg(feature = "serve")]
    let cli_oneshot = cli.command.is_some()
        && !matches!(
            cli.command,
            Some(Commands::Serve(_)) | Some(Commands::CockpitRunner(_))
        );
    #[cfg(not(feature = "serve"))]
    let cli_oneshot = cli.command.is_some();

    let result = match cli.command {
        Some(Commands::Add(args)) => cli::add::run(&profile, *args).await,
        Some(Commands::List(args)) => cli::list::run(&profile, args).await,
        Some(Commands::Remove(args)) => cli::remove::run(&profile, args).await,
        Some(Commands::Send(args)) => cli::send::run(&profile, args).await,
        Some(Commands::Status(args)) => cli::status::run(&profile, args).await,
        Some(Commands::Session { command }) => cli::session::run(&profile, command).await,
        Some(Commands::Group { command }) => cli::group::run(&profile, command).await,
        Some(Commands::Profile { command }) => cli::profile::run(command).await,
        Some(Commands::Project { command }) => {
            cli::project::run(&profile, profile_explicit, command).await
        }
        Some(Commands::Worktree { command }) => cli::worktree::run(&profile, command).await,
        #[cfg(feature = "serve")]
        Some(Commands::Serve(args)) => cli::serve::run(&profile, args).await,
        #[cfg(feature = "serve")]
        Some(Commands::Url(args)) => cli::url::run(args),
        #[cfg(feature = "serve")]
        Some(Commands::Cockpit { command }) => cli::cockpit::run(command).await,
        #[cfg(feature = "serve")]
        Some(Commands::CockpitRunner(args)) => agent_of_empires::cockpit::runner::run(*args).await,
        None => {
            // Fold the drift notice into the existing startup-warning channel
            // so the TUI surfaces both (debug-log + drift, if both fire) in a
            // single modal instead of stacking two dialogs.
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

    // Emit `process_start` for a short-lived CLI command AFTER it runs, so the
    // command's own output isn't delayed. Throttled to once per install per
    // day and awaited with a hard timeout so a dead endpoint can't hang exit;
    // a no-op unless the user opted in, so default-off users pay nothing.
    if cli_oneshot {
        agent_of_empires::telemetry::flush_cli_process_start().await;
    }

    result
}
