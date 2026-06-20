//! End-to-end tests for Agent of Empires.
//!
//! These tests exercise the full `aoe` binary -- both TUI mode (via tmux) and
//! CLI subcommands (via subprocess). They catch startup failures, rendering
//! bugs, config resolution errors, and full-flow regressions that unit and
//! integration tests miss.
//!
//! # Running
//!
//! ```sh
//! cargo test --test e2e              # run all e2e tests
//! cargo test --test e2e -- --nocapture  # with screen dumps on failure
//! ```
//!
//! TUI tests require tmux and are skipped automatically if it is not installed.
//! Docker-dependent tests are `#[ignore]` and require a running Docker daemon.

mod harness;

mod acp_focus_isolation_e2e;
mod acp_orphan_runner_recovery_e2e;
mod acp_session_log_tee_e2e;
mod acp_tool_cards_e2e;
mod archive_restore;
mod archive_structured;
mod cli;
mod command_palette;
mod errors;
mod filewatch_config_malformed;
mod filewatch_config_profile_removal;
mod filewatch_config_profile_switch;
mod filewatch_config_tui;
mod filewatch_tui_burst_reload;
mod filewatch_tui_dynamic_profile;
mod filewatch_tui_reload;
mod intro;
mod logs;
mod new_session;
mod plugins;
mod profile_picker;
mod project_registry;
mod sandbox;
mod serve;
mod settings;
mod tool_sessions;
mod tui_launch;
mod unified_view;
mod update_command;
