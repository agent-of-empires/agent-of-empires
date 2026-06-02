//! CLI argument definitions for documentation generation
//!
//! This module contains the CLI struct definitions used by clap.
//! They're separated from main.rs so xtask can generate documentation.

use clap::{Parser, Subcommand};
use clap_complete::Shell;

use super::add::AddArgs;
#[cfg(feature = "serve")]
use super::cockpit::CockpitCommands;
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

    /// Attach to a remote cockpit daemon instead of using the local
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

    /// Start a web dashboard for remote session access
    #[cfg(feature = "serve")]
    Serve(ServeArgs),

    /// Print the current dashboard URL of a running `aoe serve` daemon
    #[cfg(feature = "serve")]
    Url(UrlArgs),

    /// Cockpit (ACP-based native agent rendering) management.
    #[cfg(feature = "serve")]
    Cockpit {
        #[command(subcommand)]
        command: CockpitCommands,
    },

    /// Internal: per-cockpit-worker shim spawned by `aoe serve`. Owns the
    /// agent subprocess and outlives the daemon so workers survive
    /// `aoe serve --stop`. Hidden from help.
    #[cfg(feature = "serve")]
    #[command(name = "__cockpit-runner", hide = true)]
    CockpitRunner(Box<crate::cockpit::runner::CockpitRunnerArgs>),

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
