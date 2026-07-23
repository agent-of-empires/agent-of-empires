//! Agent of Empires - Terminal session manager for AI coding agents

use agent_of_empires::cli::{self, Cli, Commands};
use agent_of_empires::logging::{self, LogConfig, ProcessContext, SubscriberTarget};
use agent_of_empires::migrations;
use agent_of_empires::tui;
use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
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

/// When the `aoe.web` plugin is disabled, a fresh `aoe serve` start behaves as
/// an unrecognized subcommand rather than starting the dashboard (the dashboard
/// surface is a plugin, so a disabled plugin means the command is not available).
/// The daemon lifecycle verbs (`--stop` / `--status` / `--restart`) stay usable
/// so a running daemon can always be inspected and brought down. Returns the
/// clap error to raise, or `None` when the invocation is allowed. Only the
/// caller calls `.exit()`, so the decision stays unit-testable.
#[cfg(feature = "serve")]
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
    // Hidden internal helper for the VT live-preview path (`[tmux] vt_live`,
    // default on): `aoe __vt-pipe <socket>` forwards a tmux pipe-pane stream to
    // a unix socket. Handled before clap so it never appears on the CLI/docs
    // surface.
    {
        let mut a = std::env::args();
        let _ = a.next();
        if a.next().as_deref() == Some("__vt-pipe") {
            let sock = a.next().unwrap_or_default();
            return agent_of_empires::tui::run_vt_pipe(&sock).map_err(Into::into);
        }
    }

    // Hidden internal helper for on-demand smart rename:
    // `aoe __smart-rename [--force] <profile> <session-id>` runs the one-shot
    // title generator for a session and writes the title back to storage.
    // Spawned detached by the status pollers on a session's first
    // `Running -> Idle` edge (no `--force`), and by the TUI "Auto-name now"
    // action (`--force`, to bypass the disabled setting per #3039). Handled
    // before clap so it never appears on the CLI/docs surface. Best-effort: any
    // failure just leaves the auto-generated name in place.
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

    // Parse the core clap tree first. On success (every valid core command,
    // including the app-data-free ones like completion/init/agents) this never
    // touches the plugin registry. Only an error, --help/--version, or an
    // unknown subcommand falls through to the augmented tree, which grafts
    // active plugins' commands (loading the registry); there a grafted plugin
    // command is dispatched to the plugin handler, and core wins name conflicts.
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

    // With the `aoe.web` plugin disabled, a fresh `aoe serve` start is treated
    // as an unrecognized subcommand. Done here, before any logging/app-dir side
    // effects, so a rejected start creates no serve log or ProcessContext.
    #[cfg(feature = "serve")]
    if let Some(err) = serve_unavailable_error(&cli) {
        err.exit();
    }

    // If the user passed --daemon-url, mirror the value into the env
    // var so the acp::client::discovery layer (used by both the
    // remote TUI home and the `aoe acp *` verbs) picks it up
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
                // Loaded only for the `[logging]` section; commands that
                // don't reach this block (`aoe completion`, `aoe init`,
                // `aoe agents`, …) never call `get_app_dir()` as a side
                // effect.
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
                // Only the serve daemon multiplexes many sessions, so it is
                // the one process that tees session-scoped tracing into each
                // session's acp-workers/<id>.log (#1864). The acp module is
                // serve-gated, so the tee only exists in serve builds.
                #[cfg(feature = "serve")]
                let session_tee = if matches!(
                    ctx,
                    ProcessContext::ServeForeground | ProcessContext::ServeDaemonChild
                ) {
                    Some(agent_of_empires::acp::session_tee::SessionTeeLayer::new())
                } else {
                    None
                };
                #[cfg(not(feature = "serve"))]
                let session_tee: Option<logging::TeeLayer> = None;
                let res = logging::init_subscriber_with_options(
                    resolution.target,
                    filter,
                    log_cfg.show_spans,
                    session_tee,
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

    // Record which CLI subcommand ran for opt-in telemetry, before dispatch so
    // early-returning commands (e.g. `aoe update`, `aoe telemetry`) are counted
    // too. A true no-op unless the install is opted in: `track_cli_command`
    // gates on a non-creating app-dir check first, so app-data-free commands
    // (`aoe completion`, `aoe init`, ...) never materialize the app dir and keep
    // working in read-only / sandboxed (Nix) environments. Skipped for the
    // detached `--daemon-child` re-exec so `aoe serve --daemon` counts the
    // user's invocation once, not the machinery fork. The once-per-day flush is
    // bounded so a dead endpoint can never hang the command.
    if !is_daemon_child {
        if let Some(name) = cli.command.as_ref().and_then(cli::command_name) {
            agent_of_empires::telemetry::track_cli_command(name).await;
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
        Some(Commands::Settings { command }) => return cli::settings::run(command),
        Some(Commands::Telemetry { command }) => return cli::telemetry::run(command),
        Some(Commands::Mcp { command }) => {
            let profile = cli.profile.clone().unwrap_or_default();
            return cli::mcp::run(&profile, command).await;
        }
        Some(Commands::Uninstall(args)) => return cli::uninstall::run(args).await,
        Some(Commands::Update(args)) => return cli::update::run(args).await,
        // Pure redirect; needs no app data, so it must short-circuit before
        // config/migration prework that can fail in constrained environments.
        Some(Commands::Stop { .. }) => return cli::killall::stop_trap(),
        _ => {}
    }

    let profile_explicit = cli.profile.is_some();
    let profile = cli.profile.unwrap_or_default();

    // TUI mode handles migrations with a spinner; CLI runs them silently
    if cli.command.is_some() {
        migrations::run_migrations()?;
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
        #[cfg(feature = "serve")]
        Some(Commands::Serve(args)) => cli::serve::run(&profile, args).await,
        #[cfg(feature = "serve")]
        Some(Commands::Url(args)) => cli::url::run(args),
        #[cfg(feature = "serve")]
        Some(Commands::Acp { command }) => cli::acp::run(command).await,
        #[cfg(feature = "serve")]
        Some(Commands::AcpRunner(args)) => agent_of_empires::process::runner::run(*args).await,
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

    result
}
