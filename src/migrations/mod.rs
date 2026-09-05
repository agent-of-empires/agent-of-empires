//! Data migrations for handling breaking changes across versions.
//!
//! Each migration is a one-time transformation that runs when upgrading from
//! an older version. Migrations are numbered sequentially and run in order.
//!
//! To add a new migration:
//! 1. Create a new module `vNNN_description.rs`
//! 2. Implement the migration function
//! 3. Add it to the `MIGRATIONS` array below

pub mod progress;
mod v001_xdg_linux;
mod v002_seed_sandbox_from_volumes;
mod v003_yolo_mode_config;
mod v004_unified_environment;
mod v005_cockpit_defaults;
mod v006_unlimited_cockpit_history;
mod v007_serve_log_to_legacy;
mod v008_lock_in_default_profile;
mod v009_update_check_mode;
mod v010_drop_legacy_live_send_exit_chord;
mod v011_relocate_sandbox_image;
mod v012_acp_rename;
mod v013_strip_profile_theme;
mod v014_rename_default_theme;
mod v015_rewrite_hook_strings;
mod v016_clear_archived_tmux_gone_error;
mod v017_rewrite_hook_strings_for_per_user_base;
mod v018_strip_codex_config_toml_hooks;
mod v019_move_acp_defaults_to_acp;
mod v020_move_tui_branch_suffix_to_row_tag;
mod v021_split_app_state_to_state_toml;
mod v022_prune_tuning_settings;
mod v023_clear_structured_container_error;
mod v024_backfill_detect_as;
mod v025_reenable_confirm_delete;
mod v026_repoint_acp_default_agent;
pub(crate) mod v027_isolate_sandbox_stores;

use anyhow::Result;
use std::fs;
use tracing::{debug, info};

const CURRENT_VERSION: u32 = 27;
const VERSION_FILE: &str = ".schema_version";

struct Migration {
    version: u32,
    name: &'static str,
    run: fn() -> Result<()>,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "xdg_linux",
        run: v001_xdg_linux::run,
    },
    Migration {
        version: 2,
        name: "seed_sandbox_from_volumes",
        run: v002_seed_sandbox_from_volumes::run,
    },
    Migration {
        version: 3,
        name: "yolo_mode_config",
        run: v003_yolo_mode_config::run,
    },
    Migration {
        version: 4,
        name: "unified_environment",
        run: v004_unified_environment::run,
    },
    Migration {
        version: 5,
        name: "acp_defaults",
        run: v005_cockpit_defaults::run,
    },
    Migration {
        version: 6,
        name: "unlimited_cockpit_history",
        run: v006_unlimited_cockpit_history::run,
    },
    Migration {
        version: 7,
        name: "serve_log_to_legacy",
        run: v007_serve_log_to_legacy::run,
    },
    Migration {
        version: 8,
        name: "lock_in_default_profile",
        run: v008_lock_in_default_profile::run,
    },
    Migration {
        version: 9,
        name: "update_check_mode",
        run: v009_update_check_mode::run,
    },
    Migration {
        version: 10,
        name: "drop_legacy_live_send_exit_chord",
        run: v010_drop_legacy_live_send_exit_chord::run,
    },
    Migration {
        version: 11,
        name: "relocate_sandbox_image",
        run: v011_relocate_sandbox_image::run,
    },
    Migration {
        version: 12,
        name: "acp_rename",
        run: v012_acp_rename::run,
    },
    Migration {
        version: 13,
        name: "strip_profile_theme",
        run: v013_strip_profile_theme::run,
    },
    Migration {
        version: 14,
        name: "rename_default_theme",
        run: v014_rename_default_theme::run,
    },
    Migration {
        version: 15,
        name: "rewrite_hook_strings",
        run: v015_rewrite_hook_strings::run,
    },
    Migration {
        version: 16,
        name: "clear_archived_tmux_gone_error",
        run: v016_clear_archived_tmux_gone_error::run,
    },
    Migration {
        version: 17,
        name: "rewrite_hook_strings_for_per_user_base",
        run: v017_rewrite_hook_strings_for_per_user_base::run,
    },
    Migration {
        version: 18,
        name: "strip_codex_config_toml_hooks",
        run: v018_strip_codex_config_toml_hooks::run,
    },
    Migration {
        version: 19,
        name: "move_acp_defaults_to_acp",
        run: v019_move_acp_defaults_to_acp::run,
    },
    Migration {
        version: 20,
        name: "move_tui_branch_suffix_to_row_tag",
        run: v020_move_tui_branch_suffix_to_row_tag::run,
    },
    Migration {
        version: 21,
        name: "split_app_state_to_state_toml",
        run: v021_split_app_state_to_state_toml::run,
    },
    Migration {
        version: 22,
        name: "prune_tuning_settings",
        run: v022_prune_tuning_settings::run,
    },
    Migration {
        version: 23,
        name: "clear_structured_container_error",
        run: v023_clear_structured_container_error::run,
    },
    Migration {
        version: 24,
        name: "backfill_detect_as",
        run: v024_backfill_detect_as::run,
    },
    Migration {
        version: 25,
        name: "reenable_confirm_delete",
        run: v025_reenable_confirm_delete::run,
    },
    Migration {
        version: 26,
        name: "repoint_acp_default_agent",
        run: v026_repoint_acp_default_agent::run,
    },
    Migration {
        version: 27,
        name: "isolate_sandbox_stores",
        run: v027_isolate_sandbox_stores::run,
    },
];

/// The data-schema version this build targets, i.e. the version every install
/// converges to after a successful startup (migration failures abort boot, so a
/// running install is always at this version). Surfaced in telemetry as a coarse
/// version-health signal; see `crate::telemetry`.
pub fn current_schema_version() -> u32 {
    CURRENT_VERSION
}

/// Check whether there are any pending migrations to run.
pub fn has_pending_migrations() -> bool {
    get_current_version() < CURRENT_VERSION
}

/// Run all pending migrations silently. Call this early in app startup.
/// Move this session's sandbox store into the private layout, if it is still
/// on the shared one. Called from the container path so the copy is paid by
/// the session that needs it rather than by every pending row on any `aoe`
/// start.
///
/// `reporter` is how a caller with a screen narrates the copy. Nothing passes
/// one yet: [`migrate_sandbox_store_for`] installs `tracing_reporter`, which
/// reaches the log and, under `ProcessContext::Tui`, only the log
/// (`logging.rs` forces a file sink there). So a large store still copies with
/// nothing drawn, which is #3757's boot hang relocated to attach rather than
/// removed. This parameter exists so the TUI and CLI launch paths can close
/// that; until one does, the gap is real and the log is the only trail.
///
/// A failure here is reported by the caller and does not block the launch:
/// a row that did not move stays on its shared store and is retried.
pub fn migrate_sandbox_store_for_with(
    id: &str,
    reporter: Option<progress::Reporter>,
) -> Result<()> {
    if get_current_version() < 27 {
        return Ok(());
    }
    let _installed = progress::install(reporter);
    v027_isolate_sandbox_stores::migrate_instance(id)
}

/// [`migrate_sandbox_store_for_with`] using the process-wide default reporter,
/// so the copy narrates itself wherever one is configured.
pub fn migrate_sandbox_store_for(id: &str) -> Result<()> {
    migrate_sandbox_store_for_with(id, Some(progress::tracing_reporter()))
}

pub fn run_migrations() -> Result<()> {
    run_migrations_with(None)
}

/// Run all pending migrations, sending [`progress::Event`]s to `reporter` so a
/// long one (store copies, container probes) reads as work, not a hang.
///
/// A still-pending sandbox store move is *not* retried here: v027's rows move
/// when their session next needs a container, or all at once under
/// [`run_migrations_announced`] for `aoe migrate`. This path only advances the
/// schema version and reports the migrations it actually runs.
pub fn run_migrations_with(reporter: Option<progress::Reporter>) -> Result<()> {
    run_migrations_inner(reporter, false)
}

/// [`run_migrations_with`] for an explicit `aoe migrate`: a pending sandbox
/// store move also narrates what is still pending and why.
pub fn run_migrations_announced(reporter: Option<progress::Reporter>) -> Result<()> {
    run_migrations_inner(reporter, true)
}

fn run_migrations_inner(reporter: Option<progress::Reporter>, announce: bool) -> Result<()> {
    let _installed = progress::install(reporter);
    let current = get_current_version();
    debug!("Current schema version: {}", current);

    if current > CURRENT_VERSION {
        anyhow::bail!(
            "data schema version {current} is newer than this build supports ({CURRENT_VERSION}); refusing to downgrade"
        );
    }
    if current == CURRENT_VERSION {
        return v027_isolate_sandbox_stores::reconcile_pending(announce);
    }

    let pending: Vec<&Migration> = MIGRATIONS
        .iter()
        .filter(|migration| migration.version > current)
        .collect();
    for (index, migration) in pending.iter().enumerate() {
        let start = std::time::Instant::now();
        info!(
            target: "migrations",
            version = migration.version,
            name = migration.name,
            "running migration"
        );
        progress::report(progress::Event::Started {
            version: migration.version,
            name: migration.name,
            position: index + 1,
            total: pending.len(),
        });
        (migration.run)()?;
        set_version(migration.version)?;
        progress::report(progress::Event::Finished {
            version: migration.version,
            elapsed: start.elapsed(),
        });
        info!(
            target: "migrations",
            version = migration.version,
            name = migration.name,
            duration_ms = start.elapsed().as_millis() as u64,
            "migration completed"
        );
    }

    Ok(())
}

/// Get the schema version from the selected app directory.
fn get_current_version() -> u32 {
    crate::session::get_app_dir()
        .ok()
        .and_then(|dir| fs::read_to_string(dir.join(VERSION_FILE)).ok())
        .and_then(|content| content.trim().parse::<u32>().ok())
        .unwrap_or(0)
}

/// Write the version to the current app directory.
fn set_version(version: u32) -> Result<()> {
    let dir = crate::session::get_app_dir()?;
    let version_file = dir.join(VERSION_FILE);
    crate::session::atomic_write(&version_file, version.to_string().as_bytes())?;
    debug!("Updated schema version to {}", version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_migrations_are_sequential() {
        let mut prev = 0;
        for m in MIGRATIONS {
            assert!(
                m.version > prev,
                "Migration {} should be > {}",
                m.version,
                prev
            );
            prev = m.version;
        }
    }

    #[test]
    #[serial_test::serial]
    fn selected_app_dir_refuses_a_newer_schema() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), (CURRENT_VERSION + 1).to_string()).unwrap();

        let error = run_migrations().unwrap_err().to_string();

        assert!(error.contains("refusing to downgrade"));
    }

    #[test]
    fn test_current_version_matches_last_migration() {
        if let Some(last) = MIGRATIONS.last() {
            assert_eq!(CURRENT_VERSION, last.version);
        }
    }
}
