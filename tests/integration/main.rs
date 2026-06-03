//! Consolidated integration-test binary.
//!
//! Each previous `tests/<name>.rs` lives here as a submodule. Cargo links one
//! binary instead of 19, which cuts test-build wall time substantially.
//! Tests run as `cargo test --test integration [<module>::<test>]`.
//!
//! Per-test isolation still relies on `#[serial]` (see `serial_test`) for
//! anything that touches process-global state (env vars, tmux sessions,
//! `HOME`).

mod common;

mod config_merge;
mod config_wiring;
mod diff_integration;
mod group_persistence;
mod hooks_config;
mod migration_pipeline;
mod parallel_capture;
mod profile_management;
mod recovery_hook_timeout;
mod repo_config;
mod sandbox_integration;
mod session_id_acquisition;
mod session_lifecycle;
mod status_detection;
mod storage_concurrency;
mod tui_attach_detach;
mod update_command;
mod worktree_integration;

#[cfg(feature = "serve")]
mod cockpit_acp_smoke;

#[cfg(feature = "serve")]
mod cockpit_session_delete;

#[cfg(all(feature = "serve", debug_assertions))]
mod cockpit_midturn_resume;

#[cfg(all(feature = "serve", debug_assertions))]
mod cockpit_silent_orphan;
