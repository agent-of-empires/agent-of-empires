//! One-time transformations of persisted data, run in order on upgrade. See
//! `docs/development/adding-a-migration.md` to add one.

mod config_file;
mod legacy_scope_recovery;
pub mod progress;
pub(crate) mod recovery;
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
mod v034_trash_retention_minutes;
mod v035_custom_sort_order;
mod v036_session_creation_count;
mod v037_trash_lifecycle_id;
mod v038_prime_native_runtime;
mod v039_prime_control_path;
mod v040_worker_ownership;
mod v041_pending_purge_owners;
mod v042_reconcile_session_creation_count;
mod v043_reconcile_trash_lifecycle_id;
mod v044_reconcile_serve_passphrase_policy;
mod v045_runner_scope_ownership;
mod v046_reconcile_runtime_lineages;
mod v047_tool_retirements;
mod v048_terminal_handoffs;
mod v049_session_started_at;
mod v050_sound_volume_split;

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
use anyhow::{Context, Result};
use std::fs;
use tracing::{debug, info};

const CURRENT_VERSION: u32 = 50;
const VERSION_FILE: &str = ".schema_version";

/// Version, log name, and the one-time transformation to run.
type Migration = (u32, &'static str, fn() -> Result<()>);

const MIGRATIONS: &[Migration] = &[
    (1, "xdg_linux", v001_xdg_linux::run),
    (
        2,
        "seed_sandbox_from_volumes",
        v002_seed_sandbox_from_volumes::run,
    ),
    (3, "yolo_mode_config", v003_yolo_mode_config::run),
    (4, "unified_environment", v004_unified_environment::run),
    (5, "acp_defaults", v005_cockpit_defaults::run),
    (
        6,
        "unlimited_cockpit_history",
        v006_unlimited_cockpit_history::run,
    ),
    (7, "serve_log_to_legacy", v007_serve_log_to_legacy::run),
    (
        8,
        "lock_in_default_profile",
        v008_lock_in_default_profile::run,
    ),
    (9, "update_check_mode", v009_update_check_mode::run),
    (
        10,
        "drop_legacy_live_send_exit_chord",
        v010_drop_legacy_live_send_exit_chord::run,
    ),
    (
        11,
        "relocate_sandbox_image",
        v011_relocate_sandbox_image::run,
    ),
    (12, "acp_rename", v012_acp_rename::run),
    (13, "strip_profile_theme", v013_strip_profile_theme::run),
    (14, "rename_default_theme", v014_rename_default_theme::run),
    (15, "rewrite_hook_strings", v015_rewrite_hook_strings::run),
    (
        16,
        "clear_archived_tmux_gone_error",
        v016_clear_archived_tmux_gone_error::run,
    ),
    (
        17,
        "rewrite_hook_strings_for_per_user_base",
        v017_rewrite_hook_strings_for_per_user_base::run,
    ),
    (
        18,
        "strip_codex_config_toml_hooks",
        v018_strip_codex_config_toml_hooks::run,
    ),
    (
        19,
        "move_acp_defaults_to_acp",
        v019_move_acp_defaults_to_acp::run,
    ),
    (
        20,
        "move_tui_branch_suffix_to_row_tag",
        v020_move_tui_branch_suffix_to_row_tag::run,
    ),
    (
        21,
        "split_app_state_to_state_toml",
        v021_split_app_state_to_state_toml::run,
    ),
    (22, "prune_tuning_settings", v022_prune_tuning_settings::run),
    (
        23,
        "clear_structured_container_error",
        v023_clear_structured_container_error::run,
    ),
    (24, "backfill_detect_as", v024_backfill_detect_as::run),
    (
        25,
        "reenable_confirm_delete",
        v025_reenable_confirm_delete::run,
    ),
    (
        26,
        "repoint_acp_default_agent",
        v026_repoint_acp_default_agent::run,
    ),
    (
        27,
        "isolate_sandbox_stores",
        v027_isolate_sandbox_stores::run,
    ),
    (
        28,
        "clear_archived_live_status",
        v028_clear_archived_live_status::run,
    ),
    (
        29,
        "fold_pending_initial_turn",
        v029_fold_pending_initial_turn::run,
    ),
    (
        30,
        "global_only_profile_settings",
        v030_global_only_profile_settings::run,
    ),
    (
        31,
        "conversation_provenance",
        v031_conversation_provenance::run,
    ),
    (
        32,
        "bound_capture_exclusions",
        v032_bound_capture_exclusions::run,
    ),
    (
        33,
        "isolate_sandbox_content",
        v033_isolate_sandbox_content::run,
    ),
    (
        34,
        "trash_retention_minutes",
        v034_trash_retention_minutes::run,
    ),
    (35, "custom_sort_order", v035_custom_sort_order::run),
    (
        36,
        "session_creation_count",
        v036_session_creation_count::run,
    ),
    (37, "trash_lifecycle_id", v037_trash_lifecycle_id::run),
    (38, "prime_native_runtime", v038_prime_native_runtime::run),
    (39, "prime_control_path", v039_prime_control_path::run),
    (40, "worker_ownership", v040_worker_ownership::run),
    (41, "pending_purge_owners", v041_pending_purge_owners::run),
    (
        42,
        "reconcile_session_creation_count",
        v042_reconcile_session_creation_count::run,
    ),
    (
        43,
        "reconcile_trash_lifecycle_id",
        v043_reconcile_trash_lifecycle_id::run,
    ),
    (
        44,
        "reconcile_serve_passphrase_policy",
        v044_reconcile_serve_passphrase_policy::run,
    ),
    (
        45,
        "runner_scope_ownership",
        v045_runner_scope_ownership::run,
    ),
    (
        46,
        "reconcile_runtime_lineages",
        v046_reconcile_runtime_lineages::run,
    ),
    (47, "tool_retirements", v047_tool_retirements::run),
    (48, "terminal_handoffs", v048_terminal_handoffs::run),
    (49, "session_started_at", v049_session_started_at::run),
    (50, "sound_volume_split", v050_sound_volume_split::run),
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

/// Check compatibility before changing or stopping a managed daemon.
pub(crate) fn ensure_schema_supported() -> Result<u32> {
    let current = get_current_version();
    if current > CURRENT_VERSION {
        anyhow::bail!(
            "data schema version {current} is newer than this build supports ({CURRENT_VERSION}); refusing to downgrade"
        );
    }
    Ok(current)
}

/// The detached daemon child must observe its parent's completed migration.
pub fn assert_schema_current() -> Result<()> {
    let current = ensure_schema_supported()?;
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
    run_migrations_inner(reporter, false, None, None)
}

/// [`run_migrations_with`] for an explicit `aoe migrate`: a pending sandbox
/// store move also narrates what is still pending and why.
pub fn run_migrations_announced(reporter: Option<progress::Reporter>) -> Result<()> {
    run_migrations_inner(reporter, true, None, None)
}

pub(crate) fn reconcile_serve_launch_under_transaction(
    transaction: &crate::daemon::lifecycle::Transaction,
) -> Result<()> {
    v044_reconcile_serve_passphrase_policy::run_with_transaction(transaction)
}

pub(crate) fn run_migrations_under_exclusion(
    reporter: Option<progress::Reporter>,
    announce: bool,
    transaction: &crate::daemon::lifecycle::Transaction,
    serve: &crate::cli::serve::ServeLock,
) -> Result<()> {
    run_migrations_inner(reporter, announce, Some(transaction), Some(serve))
}

fn run_migrations_inner(
    reporter: Option<progress::Reporter>,
    announce: bool,
    transaction: Option<&crate::daemon::lifecycle::Transaction>,
    serve: Option<&crate::cli::serve::ServeLock>,
) -> Result<()> {
    let _installed = progress::install(reporter);
    let _announced = progress::install_announced(announce);
    let current = ensure_schema_supported()?;
    // Older daemons know serve.lock but not the lifecycle transaction. Hold
    // both before the first schema write; standalone callers include the TUI.
    let owned_transaction;
    let transaction = if (current < CURRENT_VERSION || announce) && transaction.is_none() {
        owned_transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(
            std::time::Duration::from_secs(10),
        )?;
        Some(&owned_transaction)
    } else {
        transaction
    };
    // Another migrator may have completed while we acquired its transaction.
    let current = ensure_schema_supported()?;
    let owned_serve;
    let serve = if (current < CURRENT_VERSION || announce) && serve.is_none() {
        owned_serve = crate::cli::serve::try_lock_serve()?.context(
            "A daemon or migration is starting or running; schema migration refused. \
             Run `aoe serve --restart` with the new binary, or stop the daemon through its manager",
        )?;
        Some(&owned_serve)
    } else {
        serve
    };
    if current < CURRENT_VERSION || announce {
        let transaction = transaction.context("Missing migration lifecycle transaction")?;
        // Drop this probe before v044 and explicit recovery take nested lifetime
        // and state exclusions. The held transaction and serve lock exclude starts.
        let _lifetime = transaction.try_lifetime()?.context(
            "A daemon still owns its lifetime; schema migration refused. \
             Run `aoe serve --restart` with the new binary, or stop the daemon through its manager",
        )?;
        anyhow::ensure!(
            crate::cli::serve::daemon_status() == crate::cli::serve::DaemonStatus::Absent,
            "Existing daemon identity is live or ambiguous; schema migration refused. \
             Run `aoe serve --restart` with the new binary, or stop the daemon through its manager"
        );
    }
    debug!("Current schema version: {}", current);
    if current == CURRENT_VERSION {
        v027_isolate_sandbox_stores::reconcile_pending(announce)?;
        v033_isolate_sandbox_content::reconcile_pending(announce)?;
    } else {
        let pending: Vec<&Migration> = MIGRATIONS
            .iter()
            .filter(|(version, ..)| *version > current)
            .collect();
        for (index, (version, name, run)) in pending.iter().enumerate() {
            let start = std::time::Instant::now();
            info!(target: "migrations", version, name, "running migration");
            progress::report(progress::Event::Started {
                version: *version,
                name,
                position: index + 1,
                total: pending.len(),
            });
            match (*version, transaction) {
                (44, Some(transaction)) => {
                    v044_reconcile_serve_passphrase_policy::run_with_exclusion(transaction, serve)?
                }
                _ => run()?,
            }
            set_version(*version)?;
            progress::report(progress::Event::Finished {
                version: *version,
                elapsed: start.elapsed(),
            });
            info!(target: "migrations", version, name, duration_ms = start.elapsed().as_millis() as u64, "migration completed");
        }
    }
    if announce {
        run_explicit_recovery(transaction, serve)?;
    }
    Ok(())
}

fn run_explicit_recovery(
    transaction: Option<&crate::daemon::lifecycle::Transaction>,
    serve: Option<&crate::cli::serve::ServeLock>,
) -> Result<()> {
    let owned_transaction;
    let transaction = match transaction {
        Some(transaction) => transaction,
        None => {
            owned_transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(
                std::time::Duration::from_secs(10),
            )?;
            &owned_transaction
        }
    };
    let owned_serve;
    let serve = match serve {
        Some(serve) => serve,
        None => {
            owned_serve = crate::cli::serve::try_lock_serve()?.ok_or_else(|| {
                anyhow::anyhow!(
                    "Daemon or migration owns serve.lock; explicit scope recovery refused"
                )
            })?;
            &owned_serve
        }
    };
    crate::cli::serve::with_inactive_serve_state(transaction, Some(serve), |_| {
        recovery::reconcile_repaired_components()
    })?;
    legacy_scope_recovery::run_under_exclusion(transaction, serve)
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
    #[serial_test::serial]
    fn pending_schema_migration_preserves_marker_under_active_daemon_exclusion() -> Result<()> {
        for borrowed in [false, true] {
            let home = tempfile::tempdir()?;
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir()?;
            std::fs::create_dir_all(&app)?;
            set_version(CURRENT_VERSION - 1)?;
            let marker = std::fs::read(app.join(VERSION_FILE))?;
            let transaction = borrowed
                .then(|| {
                    crate::daemon::lifecycle::Transaction::acquire_blocking_for(
                        std::time::Duration::from_secs(1),
                    )
                })
                .transpose()?;
            let serve = crate::cli::serve::try_lock_serve()?.unwrap();
            let _lifetime = transaction
                .as_ref()
                .map(crate::daemon::lifecycle::Transaction::lifetime)
                .transpose()?;
            let result = match &transaction {
                Some(transaction) => {
                    run_migrations_under_exclusion(None, false, transaction, &serve)
                }
                None => run_migrations(),
            };
            assert!(
                result.is_err(),
                "active daemon exclusion must block migration"
            );
            assert_eq!(std::fs::read(app.join(VERSION_FILE))?, marker);
        }
        Ok(())
    }

    #[test]
    fn test_migrations_are_sequential() {
        let mut prev = 0;
        for (version, ..) in MIGRATIONS {
            assert!(*version > prev, "Migration {version} should be > {prev}");
            prev = *version;
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

    /// Upgrading preserves the sort order, and a second run leaves state untouched.
    #[test]
    #[serial_test::serial]
    fn schema_34_advances_and_keeps_its_sort_order() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), "34").unwrap();
        let state = "sort_order = \"oldest\"\n";
        fs::write(app.join("state.toml"), state).unwrap();
        assert_eq!(
            crate::session::config::AppStateConfig::load()
                .unwrap()
                .sort_order,
            Some(crate::session::config::SortOrder::Oldest),
            "the fixture holds a non-default order the previous schema can read"
        );

        run_migrations().unwrap();
        let advanced = get_current_version();
        assert!(advanced > 34, "the version advances past 34, to {advanced}");
        assert_eq!(advanced, CURRENT_VERSION);
        assert_eq!(
            crate::session::config::AppStateConfig::load()
                .unwrap()
                .sort_order,
            Some(crate::session::config::SortOrder::Oldest)
        );
        let migrated_state = fs::read_to_string(app.join("state.toml")).unwrap();

        run_migrations().unwrap();
        assert_eq!(get_current_version(), advanced, "a second run stays put");
        assert_eq!(
            fs::read_to_string(app.join("state.toml")).unwrap(),
            migrated_state,
            "and leaves the state alone"
        );
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
            r#"[{"id":"3131313131313131","agent_session_id":"old","resume_intent":{"kind":"Use","value":"target"},"retroactive_capture_excludes":["old"]}]"#,
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
    fn schema_33_upgrade_initializes_purge_ownership() {
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
        assert_eq!(journal["version"], 3);
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

        run_migrations().unwrap();
        let storage =
            crate::session::Storage::new_for_test_path("default", app.join("sessions.json"));
        assert!(crate::session::purge_owners::protection(&storage, None).is_err());
        assert!(!app.join(crate::session::purge_owners::FILE_NAME).exists());
    }

    #[test]
    #[serial_test::serial]
    fn oldest_migration_backup_of_an_upgrade_stays_readable_by_the_previous_release() {
        // v1.16.1 typed both of these as plain strings.
        #[derive(serde::Deserialize)]
        struct Pre116 {
            #[serde(default)]
            retroactive_capture_excludes: std::collections::HashSet<String>,
            pending_initial_turn: Option<String>,
        }

        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::write(app.join(VERSION_FILE), "28").unwrap();
        fs::write(
            app.join("sessions.json"),
            r#"[{"id":"2828282828282828","retroactive_capture_excludes":["legacy-sid"],"pending_initial_turn":"go"}]"#,
        )
        .unwrap();

        run_migrations().unwrap();

        let backups = crate::session::migration_backups(&app.join("sessions.json")).unwrap();
        assert!(
            !backups.is_empty(),
            "an upgrade that retypes a field must leave a migration backup"
        );

        let before: Vec<Pre116> =
            serde_json::from_slice(&fs::read(&backups[0].1).unwrap()).unwrap();
        assert!(before[0]
            .retroactive_capture_excludes
            .contains("legacy-sid"));
        assert_eq!(before[0].pending_initial_turn.as_deref(), Some("go"));
    }

    #[test]
    #[serial_test::serial]
    fn an_explicit_migrate_acts_on_both_sandbox_reconciliations_and_a_startup_only_reports() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let _probes = v033_isolate_sandbox_content::install_test_reconcile_probes(
            |_| Ok(false),
            |_| Ok(true),
            |_| Ok(Vec::new()),
        );
        let app = crate::session::get_app_dir().unwrap();
        let home = dirs::home_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        fs::create_dir_all(home.join(".gemini/sandbox/history")).unwrap();
        fs::write(home.join(".gemini/sandbox/history/id.json"), b"legacy").unwrap();
        let mut instance =
            crate::session::Instance::new("One", temp.path().join("project").to_str().unwrap());
        instance.id = "one".into();
        instance.tool = "gemini".into();
        instance.sandbox_store_generation = 1;
        let mut row = serde_json::to_value(instance).unwrap();
        row["sandbox_info"] = serde_json::json!({
            "enabled":true, "image":"fixture", "container_name":"aoe-sandbox-fixture",
        });
        fs::write(
            app.join("sessions.json"),
            serde_json::to_vec(&[row]).unwrap(),
        )
        .unwrap();
        fs::write(app.join(VERSION_FILE), CURRENT_VERSION.to_string()).unwrap();
        crate::session::purge_owners::initialize(&app).unwrap();

        // A bare startup: both reconciliations defer, so neither commits.
        run_migrations_with(None).unwrap();
        let reported: serde_json::Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_ne!(
            reported[0]["sandbox_store_generation"],
            current_store_generation(),
            "a startup must not act on the store move it reported as pending: {reported}"
        );
        assert_eq!(
            reported[0].get("sandbox_content_policy"),
            None,
            "a startup must not isolate the content it reported as pending: {reported}"
        );
        assert!(
            home.join(".gemini/sandbox").is_dir(),
            "the shared store a startup leaves pending must stay in place"
        );

        // The explicit `aoe migrate`: the same flag, now acting on both.
        run_migrations_announced(None).unwrap();
        let acted: serde_json::Value =
            serde_json::from_slice(&fs::read(app.join("sessions.json")).unwrap()).unwrap();
        assert_eq!(
            acted[0]["sandbox_store_generation"],
            current_store_generation(),
            "an explicit migrate must move the store it reported as pending: {acted}"
        );
        assert_eq!(
            acted[0]["sandbox_content_policy"],
            serde_json::json!(crate::migrations::v033_isolate_sandbox_content::CONTENT_POLICY),
            "an explicit migrate must isolate the content it reported as pending: {acted}"
        );
        assert!(
            !home.join(".gemini/sandbox").exists(),
            "a committed move retires the shared store it copied from"
        );
    }

    /// The generation a v027-committed row carries, as the JSON the registry
    /// holds it in.
    fn current_store_generation() -> serde_json::Value {
        serde_json::json!(u64::from(
            crate::session::config::container_config::CURRENT_SANDBOX_STORE_GENERATION
        ))
    }
    #[test]
    #[serial_test::serial]
    fn ordinary_current_schema_startup_never_runs_scope_authority_recovery() -> Result<()> {
        let home = tempfile::tempdir()?;
        let _home = crate::session::test_support::isolate_app_dir_at(home.path());
        let app = crate::session::get_app_dir()?;
        crate::session::purge_owners::initialize(&app)?;
        crate::tmux::tool_retirement::initialize(&app)?;
        let store = crate::session::Storage::new_unwatched("default")?;
        let mut row = crate::session::Instance::new("old", "/tmp/repo");
        row.view = crate::session::View::Terminal;
        row.terminal_scopes = None;
        store.update(|rows, _| {
            rows.push(row.clone());
            Ok(())
        })?;
        let workers = crate::process::worker_registry::workers_dir()?;
        std::fs::write(workers.join("quarantined.json"), b"{")?;
        set_version(CURRENT_VERSION)?;
        let before = std::fs::read(store.sessions_path())?;
        let transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(
            std::time::Duration::from_secs(1),
        )?;
        let serve = crate::cli::serve::try_lock_serve()?.unwrap();
        run_migrations_under_exclusion(None, false, &transaction, &serve)?;
        assert_eq!(std::fs::read(store.sessions_path())?, before);
        assert_eq!(std::fs::read(workers.join("quarantined.json"))?, b"{");
        assert_eq!(get_current_version(), CURRENT_VERSION);
        assert!(run_migrations_under_exclusion(None, true, &transaction, &serve).is_err());
        assert_eq!(std::fs::read(store.sessions_path())?, before);
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn explicit_recovery_runs_after_current_and_just_completed_migrations() -> Result<()> {
        for version in [CURRENT_VERSION, CURRENT_VERSION - 1] {
            let home = tempfile::tempdir()?;
            let _home = crate::session::test_support::isolate_app_dir_at(home.path());
            let app = crate::session::get_app_dir()?;
            crate::session::purge_owners::initialize(&app)?;
            crate::tmux::tool_retirement::initialize(&app)?;
            let store = crate::session::Storage::new_unwatched("default")?;
            let mut row = crate::session::Instance::new("old", "/tmp/repo");
            row.view = crate::session::View::Terminal;
            row.terminal_scopes = None;
            store.update(|rows, _| {
                rows.push(row.clone());
                Ok(())
            })?;
            let mut persisted: serde_json::Value =
                serde_json::from_slice(&std::fs::read(store.sessions_path())?)?;
            persisted[0]["terminal_handoff"] = serde_json::Value::Null;
            std::fs::write(
                store.sessions_path(),
                serde_json::to_vec_pretty(&persisted)?,
            )?;
            set_version(version)?;
            let before = std::fs::read(store.sessions_path())?;
            let journal_path = app.join(crate::session::purge_owners::FILE_NAME);
            let journal_before = std::fs::read(&journal_path)?;
            let transaction = crate::daemon::lifecycle::Transaction::acquire_blocking_for(
                std::time::Duration::from_secs(1),
            )?;
            let serve = crate::cli::serve::try_lock_serve()?.unwrap();
            let unavailable = legacy_scope_recovery::test_support::deny_inventory();
            assert!(run_migrations_under_exclusion(None, true, &transaction, &serve).is_err());
            assert_eq!(unavailable.attempts(), 1);
            assert_eq!(get_current_version(), CURRENT_VERSION);
            assert_eq!(std::fs::read(store.sessions_path())?, before);
            assert_eq!(std::fs::read(journal_path)?, journal_before);
            assert!(crate::session::migration_backups(store.sessions_path())?.is_empty());
        }
        Ok(())
    }
}
