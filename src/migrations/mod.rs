//! Data migrations for handling breaking changes across versions.
//!
//! Each migration is a one-time transformation that runs when upgrading from
//! an older version. Migrations are numbered sequentially and run in order.
//!
//! To add a new migration:
//! 1. Create a new module `vNNN_description.rs`
//! 2. Implement the migration function
//! 3. Add it to the `MIGRATIONS` array below

mod config_file;
pub mod progress;
mod sessions_file;
mod store_fs;
#[cfg(test)]
mod test_cases;
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
mod v028_clear_archived_live_status;
mod v029_fold_pending_initial_turn;
mod v030_global_only_profile_settings;
mod v031_conversation_provenance;
mod v032_bound_capture_exclusions;
pub(crate) mod v033_isolate_sandbox_content;
mod v034_core_daemon_launch;
mod v035_serve_passphrase_policy;
mod v036_pending_purge_owners;
mod v037_capture_purge_runners;
mod v038_canonical_sidebar;

/// Fixtures shared by migrations that rewrite agent hook files.
#[cfg(test)]
mod hook_fixtures {
    use crate::session::test_support::EnvGuard;
    use serde_json::Value;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    pub(super) fn unset_agent_home_env() -> EnvGuard {
        EnvGuard::unset(&[
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "CURSOR_CONFIG_DIR",
            "GEMINI_CONFIG_DIR",
            "QWEN_CONFIG_DIR",
        ])
    }

    pub(super) fn setup_dirs() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let app_dir = tmp.path().join("app");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&app_dir).unwrap();
        (tmp, home, app_dir)
    }

    pub(super) fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }
}
use anyhow::Result;
use std::fs;
use tracing::{debug, info};

const CURRENT_VERSION: u32 = 38;
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
    Migration {
        version: 28,
        name: "clear_archived_live_status",
        run: v028_clear_archived_live_status::run,
    },
    Migration {
        version: 29,
        name: "fold_pending_initial_turn",
        run: v029_fold_pending_initial_turn::run,
    },
    Migration {
        version: 30,
        name: "global_only_profile_settings",
        run: v030_global_only_profile_settings::run,
    },
    Migration {
        version: 31,
        name: "conversation_provenance",
        run: v031_conversation_provenance::run,
    },
    Migration {
        version: 32,
        name: "bound_capture_exclusions",
        run: v032_bound_capture_exclusions::run,
    },
    Migration {
        version: 33,
        name: "isolate_sandbox_content",
        run: v033_isolate_sandbox_content::run,
    },
    Migration {
        version: 34,
        name: "core_daemon_launch",
        run: v034_core_daemon_launch::run,
    },
    Migration {
        version: 35,
        name: "serve_passphrase_policy",
        run: v035_serve_passphrase_policy::run,
    },
    Migration {
        version: 36,
        name: "pending_purge_owners",
        run: v036_pending_purge_owners::run,
    },
    Migration {
        version: 37,
        name: "capture_purge_runners",
        run: v037_capture_purge_runners::run,
    },
    Migration {
        version: 38,
        name: "canonical_sidebar",
        run: v038_canonical_sidebar::run,
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

/// Refuse to run against a schema this build does not own: either pending
/// migrations, or a version from a newer build.
///
/// This is the check the detached daemon child makes instead of migrating.
/// Its parent `aoe serve` ran the migrations and still holds the daemon
/// lifecycle transaction, so a migration that re-acquires that lock (v034 and
/// v035 do) would spin out the full 60s deadline and then fail the child
/// before it ever receives the transaction — the daemon would simply never
/// come up. Verifying is the fail-closed alternative.
pub fn assert_schema_current() -> Result<()> {
    let current = get_current_version();
    if current > CURRENT_VERSION {
        anyhow::bail!(
            "data schema version {current} is newer than this build supports ({CURRENT_VERSION}); refusing to downgrade"
        );
    }
    anyhow::ensure!(
        current == CURRENT_VERSION,
        "data schema version {current} is behind this build ({CURRENT_VERSION}); \
         the parent `aoe serve` must migrate before launching the daemon child"
    );
    Ok(())
}

/// Move this session's shared sandbox store and isolate its native content,
/// reporting copy progress to the caller. Unproven content remains pending.
pub(crate) fn migrate_sandbox_store_for_with(
    id: &str,
    reporter: Option<progress::Reporter>,
    store: &dyn crate::session::SessionStore,
    runtime: &crate::containers::ContainerRuntime,
) -> Result<()> {
    if get_current_version() < 27 {
        return Ok(());
    }
    let _installed = progress::install(reporter);
    v027_isolate_sandbox_stores::migrate_instance(id, store, runtime)?;
    v033_isolate_sandbox_content::migrate_instance(id)
}

/// [`migrate_sandbox_store_for_with`] with the container probes injected, for
/// a test that drives the launch-time move with no container runtime.
#[cfg(test)]
pub(crate) fn migrate_sandbox_store_for_test(
    id: &str,
    reporter: Option<progress::Reporter>,
    is_running: &dyn Fn(&str) -> Result<bool>,
    reap: &dyn Fn(&str) -> Result<bool>,
) -> Result<()> {
    let _installed = progress::install(reporter);
    v027_isolate_sandbox_stores::migrate_instance_with(id, is_running, reap, None)
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
    let _announced = progress::install_announced(announce);
    let current = get_current_version();
    debug!("Current schema version: {}", current);

    if current > CURRENT_VERSION {
        anyhow::bail!(
            "data schema version {current} is newer than this build supports ({CURRENT_VERSION}); refusing to downgrade"
        );
    }
    if current == CURRENT_VERSION {
        v027_isolate_sandbox_stores::reconcile_pending(announce)?;
        v033_isolate_sandbox_content::reconcile_pending(announce)?;
        return v037_capture_purge_runners::reconcile();
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
    #[serial_test::serial]
    fn the_daemon_child_check_refuses_a_schema_it_would_have_to_migrate() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), (CURRENT_VERSION - 1).to_string()).unwrap();

        let error = assert_schema_current().unwrap_err().to_string();

        assert!(
            error.contains("behind this build"),
            "a behind schema must be refused, not silently migrated: {error}"
        );
    }

    #[test]
    #[serial_test::serial]
    fn the_daemon_child_check_passes_on_a_migrated_schema() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), CURRENT_VERSION.to_string()).unwrap();

        assert!(assert_schema_current().is_ok());
    }

    #[test]
    #[serial_test::serial]
    fn the_daemon_child_check_refuses_a_newer_schema_too() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), (CURRENT_VERSION + 1).to_string()).unwrap();

        assert!(assert_schema_current()
            .unwrap_err()
            .to_string()
            .contains("refusing to downgrade"));
    }

    #[test]
    fn test_current_version_matches_last_migration() {
        if let Some(last) = MIGRATIONS.last() {
            assert_eq!(CURRENT_VERSION, last.version);
        }
    }

    #[test]
    #[serial_test::serial]
    fn schema_31_content_isolation_still_receives_upstream_provenance() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), "31").unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"agent_session_id":"old","resume_intent":{"kind":"Use","value":"target"},"retroactive_capture_excludes":["old"]}]"#,
        )
        .unwrap();

        run_migrations().unwrap();

        let rows: serde_json::Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(rows[0]["agent_session_id"], "old");
        assert_eq!(rows[0]["agent_session_binding"]["provenance"], "unknown");
        assert!(rows[0]["agent_session_binding"]["execution"].is_null());
        assert_eq!(rows[0]["resume_binding"]["session_id"], "target");
        assert_eq!(
            rows[0]["retroactive_capture_excludes"][0]["session_id"],
            "old"
        );
        assert_eq!(get_current_version(), CURRENT_VERSION);
    }

    #[test]
    #[serial_test::serial]
    fn schema_33_upgrade_initializes_then_captures_purge_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), "33").unwrap();

        run_migrations().unwrap();

        assert_eq!(get_current_version(), CURRENT_VERSION);
        let journal: serde_json::Value = serde_json::from_slice(
            &fs::read(app.join(crate::session::purge_owners::FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert_eq!(journal["version"], 2);
        assert_eq!(journal["owners"], serde_json::json!([]));
    }

    #[test]
    #[serial_test::serial]
    fn current_schema_never_recreates_a_lost_purge_journal() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), CURRENT_VERSION.to_string()).unwrap();

        let error = run_migrations().unwrap_err().to_string();

        assert!(error.contains("Pending purge ownership journal is missing"));
        assert!(!app.join(crate::session::purge_owners::FILE_NAME).exists());
    }
}
