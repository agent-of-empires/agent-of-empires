//! Consolidated integration-test binary.
//!
//! Each previous `tests/<name>.rs` lives here as a submodule. Cargo links one
//! binary instead of one per file, which cuts test-build wall time
//! substantially. New integration tests go here, not as loose files under
//! `tests/`. Tests run as `cargo test --test integration [<module>::<test>]`.
//!
//! Environment writers use the default `#[serial]` key; all other tests use
//! default `#[parallel]` so readers cannot overlap a writer. Fixtures that
//! poison concurrent consumers beyond that lock still need separate processes:
//! `branch_exists_spawn_failure.rs` isolates `PATH`, and
//! `filewatch_degradation.rs` isolates `AOE_FILE_WATCH=off`.
//!
//! Modules behind `#[cfg(debug_assertions)]` use test hooks and helpers that
//! only debug builds compile, so release test builds (the Nix checks) skip them.

mod common;
mod home_isolation;

mod daemon_client;
#[cfg(debug_assertions)]
mod hidden_env_batch;
mod hooks_config;
mod migration_pipeline;
mod profile_management;
mod recovery_hook_timeout;
mod repo_config;
mod session_id_acquisition;
mod status_detection;
#[cfg(debug_assertions)]
mod storage_concurrency;
mod terminal_smart_rename;
mod tmux_reachability;
mod tui_attach_detach;
mod update_command;
mod worktree_integration;

mod acp_mcp;

mod acp_smoke;

mod acp_session_delete;

mod acp_model_respawn;

#[cfg(debug_assertions)]
mod acp_midturn_resume;

#[cfg(debug_assertions)]
mod acp_silent_orphan;

#[cfg(debug_assertions)]
mod acp_runner_control;
mod acp_runner_orphan;
mod agent_lifecycle_cli;
mod build_cache_config;
mod build_version_rerun;
#[cfg(debug_assertions)]
mod daemon_core_web_optional;
mod filewatch_config_editor_burst;
#[cfg(debug_assertions)]
mod log_filter_watcher_migration;
mod no_stale_doc_refs;
mod plugin_install;
#[cfg(debug_assertions)]
mod project_create_dedupe;
#[cfg(debug_assertions)]
mod serve_cityhall_lockdown;
#[cfg(debug_assertions)]
mod serve_daemon_session_id_drain;
#[cfg(debug_assertions)]
mod serve_disk_reload_helper_equivalence;
#[cfg(debug_assertions)]
mod serve_dynamic_profile_rewire;
#[cfg(debug_assertions)]
mod serve_filewatch_propagation;
#[cfg(debug_assertions)]
mod serve_settings_logging;
mod telemetry;
