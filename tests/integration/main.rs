//! Consolidated integration-test binary.
//!
//! Each previous `tests/<name>.rs` lives here as a submodule. Cargo links one
//! binary instead of one per file, which cuts test-build wall time
//! substantially. New integration tests go here, not as loose files under
//! `tests/`. Tests run as `cargo test --test integration [<module>::<test>]`.
//!
//! Per-test isolation still relies on `#[serial]` (see `serial_test`) for
//! anything that touches process-global state (env vars, tmux sessions,
//! `HOME`). `#[serial]` does not exclude unmarked tests, so a test whose
//! seam poisons every concurrent subprocess spawn (for example clobbering
//! `PATH`) cannot live in this binary; `tests/branch_exists_spawn_failure.rs`
//! stays standalone for that reason.

mod common;
mod home_isolation;

mod config_merge;
mod config_wiring;
mod diff_integration;
mod group_persistence;
mod hidden_env_batch;
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
mod terminal_smart_rename;
mod tmux_reachability;
mod tui_attach_detach;
mod update_command;
mod worktree_integration;

mod acp_mcp;

mod acp_smoke;

mod acp_session_delete;

mod acp_effort_respawn;

#[cfg(debug_assertions)]
mod acp_midturn_resume;

#[cfg(debug_assertions)]
mod acp_silent_orphan;

mod acp_runner_control;
mod acp_runner_orphan;
mod agent_lifecycle_cli;
mod build_cache_config;
mod build_version_rerun;
mod daemon_core_web_optional;
mod filewatch_config_editor_burst;
mod filewatch_degradation;
mod filewatch_tui_adapter_lifetime;
mod filewatch_tui_drop_then_abort;
mod log_filter_watcher_migration;
mod no_stale_doc_refs;
mod plugin_install;
mod serve_cityhall_lockdown;
mod serve_daemon_session_id_drain;
mod serve_disk_reload_helper_equivalence;
mod serve_dns_rebinding_gate;
mod serve_dynamic_profile_rewire;
mod serve_filewatch_propagation;
mod telemetry;
