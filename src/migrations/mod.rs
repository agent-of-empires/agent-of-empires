//! Data migrations for handling breaking changes across versions.
//!
//! Each migration is a one-time transformation that runs when upgrading from
//! an older version. Migrations are numbered sequentially and run in order.
//!
//! To add a new migration:
//! 1. Create a new module `vNNN_description.rs`
//! 2. Implement the migration function
//! 3. Add it to the `MIGRATIONS` array below

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

use anyhow::Result;
use std::fs;
use std::path::PathBuf;
use tracing::{debug, info};

const CURRENT_VERSION: u32 = 12;
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

/// Run all pending migrations. Call this early in app startup.
pub fn run_migrations() -> Result<()> {
    let current = get_current_version();
    debug!("Current schema version: {}", current);

    if current >= CURRENT_VERSION {
        return Ok(());
    }

    for migration in MIGRATIONS {
        if migration.version > current {
            let start = std::time::Instant::now();
            info!(
                target: "migrations",
                version = migration.version,
                name = migration.name,
                "running migration"
            );
            (migration.run)()?;
            set_version(migration.version)?;
            info!(
                target: "migrations",
                version = migration.version,
                name = migration.name,
                duration_ms = start.elapsed().as_millis() as u64,
                "migration completed"
            );
        }
    }

    Ok(())
}

/// Get the current schema version by checking all possible locations.
fn get_current_version() -> u32 {
    for dir in get_all_possible_dirs() {
        let version_file = dir.join(VERSION_FILE);
        if let Ok(content) = fs::read_to_string(&version_file) {
            if let Ok(version) = content.trim().parse::<u32>() {
                return version;
            }
        }
    }
    0
}

/// Write the version to the current app directory.
fn set_version(version: u32) -> Result<()> {
    let dir = crate::session::get_app_dir()?;
    let version_file = dir.join(VERSION_FILE);
    fs::write(&version_file, version.to_string())?;
    debug!("Updated schema version to {}", version);
    Ok(())
}

/// Returns all directories where app data might exist (for migration discovery).
fn get_all_possible_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Some(home) = dirs::home_dir() {
        dirs.push(home.join(crate::session::APP_DIR_NAME_OTHER));
    }

    #[cfg(target_os = "linux")]
    if let Some(config_dir) = dirs::config_dir() {
        dirs.push(config_dir.join(crate::session::APP_DIR_NAME_LINUX));
    }

    dirs
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
    fn test_current_version_matches_last_migration() {
        if let Some(last) = MIGRATIONS.last() {
            assert_eq!(CURRENT_VERSION, last.version);
        }
    }
}
