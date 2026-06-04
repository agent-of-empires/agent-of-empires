//! Agent of Empires library - Core functionality for the terminal session manager

#[cfg(feature = "serve")]
pub mod acp;
pub mod agents;
pub mod build_info;
pub mod claude_settings;
pub mod cli;
pub mod containers;
pub mod file_watch;
pub mod git;
pub mod github;
pub mod hooks;
pub mod logging;
pub mod migrations;
pub mod process;
#[cfg(feature = "serve")]
pub mod server;
pub mod session;
pub mod sound;
mod status_hooks;
pub mod task_util;
pub mod telemetry;
pub mod terminal;
pub mod tmux;
pub mod tui;
pub mod update;
