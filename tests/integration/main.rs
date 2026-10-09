//! Consolidated integration-test binary.
//!
//! Public integration modules share one binary to reduce linking cost. Run
//! them with `cargo test --test integration [<module>::<test>]`. Native runner
//! transport cases use the private issuer through `src/lib.rs`; build `aoe`
//! with the same features, then run `cargo test --lib runner_tests`.
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
mod hooks_cli;
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
mod tmux_send_keys;
mod update_command;
mod worktree_integration;

mod acp_mcp;

mod acp_smoke;

mod acp_session_delete;

mod acp_model_respawn;
mod acp_session_import;

mod acp_provider_respawn;

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
mod serve_settings_layers;
#[cfg(debug_assertions)]
mod serve_settings_logging;
mod telemetry;
