//! CLI argument definitions for documentation generation
//!
//! This module contains the CLI struct definitions used by clap.
//! They're separated from main.rs so xtask can generate documentation.

use clap::{Parser, Subcommand};
use clap_complete::Shell;

#[cfg(feature = "serve")]
use super::acp::AcpCommands;
use super::add::AddArgs;
use super::extract_session_id::ExtractSessionIdArgs;
use super::group::GroupCommands;
use super::init::InitArgs;
use super::list::ListArgs;
#[cfg(feature = "serve")]
use super::log_level::LogLevelArgs;
use super::logs::LogsArgs;
use super::profile::ProfileCommands;
use super::project::ProjectCommands;
use super::remove::RemoveArgs;
use super::send::SendArgs;
#[cfg(feature = "serve")]
use super::serve::ServeArgs;
use super::session::SessionCommands;
use super::sounds::SoundsCommands;
use super::status::StatusArgs;
use super::telemetry::TelemetryCommands;
use super::theme::ThemeCommands;
use super::tmux::TmuxCommands;
use super::uninstall::UninstallArgs;
use super::update::UpdateArgs;
#[cfg(feature = "serve")]
use super::url::UrlArgs;
use super::worktree::WorktreeCommands;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(name = "aoe")]
#[command(about = "Terminal session manager for AI coding agents")]
#[command(version = VERSION)]
#[command(
    long_about = "Agent of Empires (aoe) is a terminal session manager that uses tmux to help \
    you manage and monitor AI coding agents like Claude Code and OpenCode.\n\n\
    Run without arguments to launch the TUI dashboard."
)]
pub struct Cli {
    /// Profile to use (separate workspace with its own sessions)
    #[arg(short = 'p', long, global = true, env = "AGENT_OF_EMPIRES_PROFILE")]
    pub profile: Option<String>,

    /// Attach to a remote agent daemon instead of using the local
    /// session list. Equivalent to setting `AOE_DAEMON_URL`; pair with
    /// `AOE_DAEMON_TOKEN` for the bearer token. Only meaningful at the
    /// no-subcommand `aoe` invocation (the TUI dashboard); ignored
    /// otherwise.
    #[arg(long, global = true, env = "AOE_DAEMON_URL")]
    pub daemon_url: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Add a new session
    Add(Box<AddArgs>),

    /// List supported agents and their install status
    Agents,

    /// Initialize .agent-of-empires/config.toml in a repository
    Init(InitArgs),

    /// List all sessions
    #[command(alias = "ls")]
    List(ListArgs),

    /// View the configured AoE log file with a pretty viewer
    Logs(LogsArgs),

    /// Get or set the running daemon's log filter at runtime.
    /// Pass a bare level (debug/info/...) for the safe expansion, or
    /// `--filter <expr>` for raw EnvFilter syntax. `--get` prints the
    /// current filter. Changes are ephemeral and lost on daemon restart.
    #[cfg(feature = "serve")]
    LogLevel(LogLevelArgs),

    /// Remove a session
    #[command(alias = "rm")]
    Remove(RemoveArgs),

    /// Send a message to a running agent session
    Send(SendArgs),

    /// Show session status summary
    Status(StatusArgs),

    /// Manage session lifecycle (start, stop, attach, etc.)
    Session {
        #[command(subcommand)]
        command: SessionCommands,
    },

    /// Manage groups for organizing sessions
    Group {
        #[command(subcommand)]
        command: GroupCommands,
    },

    /// Manage profiles (separate workspaces)
    Profile {
        #[command(subcommand)]
        command: Option<ProfileCommands>,
    },

    /// Manage the project registry used by multi-repo session pickers
    Project {
        #[command(subcommand)]
        command: ProjectCommands,
    },

    /// Manage git worktrees for parallel development
    Worktree {
        #[command(subcommand)]
        command: WorktreeCommands,
    },

    /// tmux integration utilities
    Tmux {
        #[command(subcommand)]
        command: TmuxCommands,
    },

    /// Manage sound effects for agent state transitions
    Sounds {
        #[command(subcommand)]
        command: SoundsCommands,
    },

    /// Manage color themes (list, export, customize)
    Theme {
        #[command(subcommand)]
        command: ThemeCommands,
    },

    /// Manage anonymous opt-in usage telemetry
    Telemetry {
        #[command(subcommand)]
        command: TelemetryCommands,
    },

    /// Start a web dashboard for remote session access
    #[cfg(feature = "serve")]
    Serve(ServeArgs),

    /// Print the current dashboard URL of a running `aoe serve` daemon
    #[cfg(feature = "serve")]
    Url(UrlArgs),

    /// Manage the ACP structured-view workers (doctor, ps, logs, prompt, approve, ...).
    #[cfg(feature = "serve")]
    Acp {
        #[command(subcommand)]
        command: AcpCommands,
    },

    /// Internal: per-acp-worker shim spawned by `aoe serve`. Owns the
    /// agent subprocess and outlives the daemon so workers survive
    /// `aoe serve --stop`. Hidden from help.
    #[cfg(feature = "serve")]
    #[command(name = "__acp-runner", hide = true)]
    AcpRunner(Box<crate::acp::runner::AcpRunnerArgs>),

    /// Internal: extract Claude's `session_id` from a hook stdin payload
    /// and write it to the sidecar file. Spawned by the host-side
    /// `SessionStart`/`UserPromptSubmit` hook. Hidden from help.
    #[command(name = "__extract-session-id", hide = true)]
    ExtractSessionId(ExtractSessionIdArgs),

    /// Uninstall Agent of Empires
    Uninstall(UninstallArgs),

    /// Update aoe to the latest release
    Update(UpdateArgs),

    /// Generate shell completions
    Completion {
        /// Shell to generate completions for
        #[arg(value_enum)]
        shell: Shell,
    },
}

/// Every command name [`command_name`] can return, used as the closed
/// allowlist when building the `cli_usage` telemetry event: any key loaded from
/// a hand-edited or corrupt `telemetry.json` that is not in this set is dropped
/// before sending, so the wire payload can only ever carry these tokens. The
/// `serve` / `url` / `acp` / `log_level` names are listed unconditionally
/// even though their variants are `serve`-feature-gated; in a TUI-only build
/// those commands cannot run, so the extra allowlist entries are simply never
/// matched. Keep in sync with [`command_name`]; the unit test asserts every
/// `command_name` output is a member.
pub const CLI_COMMAND_NAMES: &[&str] = &[
    "add",
    "agents",
    "init",
    "list",
    "logs",
    "log_level",
    "remove",
    "send",
    "status",
    "session",
    "group",
    "profile",
    "project",
    "worktree",
    "tmux",
    "sounds",
    "theme",
    "telemetry",
    "serve",
    "url",
    "acp",
    "uninstall",
    "update",
    "completion",
];

/// The canonical, telemetry-safe name of a CLI subcommand, or `None` for the
/// hidden internal commands that are machine-spawned rather than user-invoked
/// (`__acp-runner`, `__extract-session-id`) and must never be counted.
///
/// This is an exhaustive match with **no catch-all**: adding a [`Commands`]
/// variant fails to compile until it is named here, so the telemetry vocabulary
/// can never silently drift. The returned tokens are identifier-safe
/// (`snake_case`, never clap's kebab-case like `log-level`) because the
/// telemetry gateway drops map keys that do not match `^[a-z][a-z0-9_]{0,63}$`.
/// They carry no arguments, flags, or paths, only the closed command name.
pub fn command_name(command: &Commands) -> Option<&'static str> {
    Some(match command {
        Commands::Add(_) => "add",
        Commands::Agents => "agents",
        Commands::Init(_) => "init",
        Commands::List(_) => "list",
        Commands::Logs(_) => "logs",
        #[cfg(feature = "serve")]
        Commands::LogLevel(_) => "log_level",
        Commands::Remove(_) => "remove",
        Commands::Send(_) => "send",
        Commands::Status(_) => "status",
        Commands::Session { .. } => "session",
        Commands::Group { .. } => "group",
        Commands::Profile { .. } => "profile",
        Commands::Project { .. } => "project",
        Commands::Worktree { .. } => "worktree",
        Commands::Tmux { .. } => "tmux",
        Commands::Sounds { .. } => "sounds",
        Commands::Theme { .. } => "theme",
        Commands::Telemetry { .. } => "telemetry",
        #[cfg(feature = "serve")]
        Commands::Serve(_) => "serve",
        #[cfg(feature = "serve")]
        Commands::Url(_) => "url",
        #[cfg(feature = "serve")]
        Commands::Acp { .. } => "acp",
        // Internal, machine-spawned commands: never a user action, never counted.
        #[cfg(feature = "serve")]
        Commands::AcpRunner(_) => return None,
        Commands::ExtractSessionId(_) => return None,
        Commands::Uninstall(_) => "uninstall",
        Commands::Update(_) => "update",
        Commands::Completion { .. } => "completion",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Every name `command_name` returns must be in the `CLI_COMMAND_NAMES`
    /// allowlist and be an identifier-safe token (no args, no kebab-case), so a
    /// privacy reviewer can trust the only strings reaching the wire are closed
    /// command names. Parsed via clap so alias collapse (`ls` -> `list`,
    /// `rm` -> `remove`) and the real kebab/underscore mapping are exercised.
    #[test]
    fn command_name_is_allowlisted_and_identifier_safe() {
        let cases: &[(&[&str], &str)] = &[
            (&["aoe", "add", "demo"], "add"),
            (&["aoe", "agents"], "agents"),
            (&["aoe", "ls"], "list"), // alias collapses to canonical
            (&["aoe", "rm", "demo"], "remove"),
            (&["aoe", "session", "current"], "session"),
            (&["aoe", "telemetry", "status"], "telemetry"),
            (&["aoe", "update"], "update"),
            (&["aoe", "completion", "bash"], "completion"),
        ];
        for (argv, expected) in cases {
            let cli = Cli::try_parse_from(*argv).expect("parse");
            let name = command_name(cli.command.as_ref().expect("command")).expect("named");
            assert_eq!(name, *expected, "argv {argv:?}");
            assert!(
                CLI_COMMAND_NAMES.contains(&name),
                "`{name}` missing from CLI_COMMAND_NAMES"
            );
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'),
                "`{name}` is not an identifier-safe token"
            );
        }
    }

    /// Hidden, machine-spawned commands are never counted.
    #[test]
    fn hidden_commands_are_not_named() {
        let cli = Cli::try_parse_from(["aoe", "__extract-session-id"]).expect("parse");
        assert_eq!(command_name(cli.command.as_ref().expect("command")), None);
    }

    /// The compiler forces a `command_name` arm per `Commands` variant, but
    /// nothing forces a matching `CLI_COMMAND_NAMES` entry. Without this guard a
    /// contributor could add a counted command and silently drop it from the
    /// `cli_usage` payload (`build_cli_usage` filters unknown keys). Assert every
    /// visible clap subcommand is in the allowlist (subset direction: the
    /// allowlist may carry extra `serve`-only names in a TUI-only build, which is
    /// a harmless never-matched filter key). `log-level` maps to `log_level`.
    #[test]
    fn allowlist_covers_every_visible_subcommand() {
        use clap::CommandFactory;
        let visible: Vec<String> = Cli::command()
            .get_subcommands()
            .filter(|s| !s.is_hide_set())
            .map(|s| s.get_name().replace('-', "_"))
            .collect();
        assert!(!visible.is_empty(), "expected visible subcommands");
        for name in &visible {
            assert!(
                CLI_COMMAND_NAMES.contains(&name.as_str()),
                "visible subcommand `{name}` is missing from CLI_COMMAND_NAMES; \
                 it would be silently dropped from cli_usage telemetry"
            );
        }
    }
}
