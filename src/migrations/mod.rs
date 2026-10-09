//! One-time transformations of persisted data, run in order on upgrade. See
//! `docs/development/adding-a-migration.md` to add one.

mod config_file;
pub mod progress;
pub(crate) mod schema;
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
mod v042_canonical_execution_journal;

/// Fixtures shared by the migrations that rewrite agent hook files.
#[cfg(test)]
mod hook_fixtures {
    use crate::session::test_support::EnvGuard;
    use serde_json::Value;
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// Clear every agent config-dir override, so a migration's path
    /// resolution sees only the fixtures under `home` and `app_dir`.
    pub(super) fn unset_agent_home_env() -> EnvGuard {
        EnvGuard::unset(&[
            "CODEX_HOME",
            "CLAUDE_CONFIG_DIR",
            "CURSOR_CONFIG_DIR",
            "GEMINI_CONFIG_DIR",
            "QWEN_CONFIG_DIR",
        ])
    }

    /// A tempdir holding an empty `home` and app dir.
    pub(super) fn setup_dirs() -> (TempDir, PathBuf, PathBuf) {
        let tmp = TempDir::new().unwrap();
        let home = tmp.path().join("home");
        let app_dir = tmp.path().join("app");
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&app_dir).unwrap();
        (tmp, home, app_dir)
    }

    /// Write `value` as pretty JSON, creating the parent directory.
    pub(super) fn write_json(path: &Path, value: &Value) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, serde_json::to_string_pretty(value).unwrap()).unwrap();
    }
}

use anyhow::Result;
#[cfg(test)]
use std::fs;
use tracing::{debug, info};

const CURRENT_VERSION: u32 = 42;
const VERSION_FILE: &str = ".schema_version";

enum MigrationAction {
    Legacy(fn() -> Result<()>),
    Anchored(fn(&crate::session::AnchoredDir, u32) -> Result<()>),
}

type Migration = (u32, &'static str, MigrationAction);

const MIGRATIONS: &[Migration] = &[
    (1, "xdg_linux", MigrationAction::Legacy(v001_xdg_linux::run)),
    (
        2,
        "seed_sandbox_from_volumes",
        MigrationAction::Legacy(v002_seed_sandbox_from_volumes::run),
    ),
    (
        3,
        "yolo_mode_config",
        MigrationAction::Legacy(v003_yolo_mode_config::run),
    ),
    (
        4,
        "unified_environment",
        MigrationAction::Legacy(v004_unified_environment::run),
    ),
    (
        5,
        "acp_defaults",
        MigrationAction::Legacy(v005_cockpit_defaults::run),
    ),
    (
        6,
        "unlimited_cockpit_history",
        MigrationAction::Legacy(v006_unlimited_cockpit_history::run),
    ),
    (
        7,
        "serve_log_to_legacy",
        MigrationAction::Legacy(v007_serve_log_to_legacy::run),
    ),
    (
        8,
        "lock_in_default_profile",
        MigrationAction::Legacy(v008_lock_in_default_profile::run),
    ),
    (
        9,
        "update_check_mode",
        MigrationAction::Legacy(v009_update_check_mode::run),
    ),
    (
        10,
        "drop_legacy_live_send_exit_chord",
        MigrationAction::Legacy(v010_drop_legacy_live_send_exit_chord::run),
    ),
    (
        11,
        "relocate_sandbox_image",
        MigrationAction::Legacy(v011_relocate_sandbox_image::run),
    ),
    (
        12,
        "acp_rename",
        MigrationAction::Legacy(v012_acp_rename::run),
    ),
    (
        13,
        "strip_profile_theme",
        MigrationAction::Legacy(v013_strip_profile_theme::run),
    ),
    (
        14,
        "rename_default_theme",
        MigrationAction::Legacy(v014_rename_default_theme::run),
    ),
    (
        15,
        "rewrite_hook_strings",
        MigrationAction::Legacy(v015_rewrite_hook_strings::run),
    ),
    (
        16,
        "clear_archived_tmux_gone_error",
        MigrationAction::Legacy(v016_clear_archived_tmux_gone_error::run),
    ),
    (
        17,
        "rewrite_hook_strings_for_per_user_base",
        MigrationAction::Legacy(v017_rewrite_hook_strings_for_per_user_base::run),
    ),
    (
        18,
        "strip_codex_config_toml_hooks",
        MigrationAction::Legacy(v018_strip_codex_config_toml_hooks::run),
    ),
    (
        19,
        "move_acp_defaults_to_acp",
        MigrationAction::Legacy(v019_move_acp_defaults_to_acp::run),
    ),
    (
        20,
        "move_tui_branch_suffix_to_row_tag",
        MigrationAction::Legacy(v020_move_tui_branch_suffix_to_row_tag::run),
    ),
    (
        21,
        "split_app_state_to_state_toml",
        MigrationAction::Legacy(v021_split_app_state_to_state_toml::run),
    ),
    (
        22,
        "prune_tuning_settings",
        MigrationAction::Legacy(v022_prune_tuning_settings::run),
    ),
    (
        23,
        "clear_structured_container_error",
        MigrationAction::Legacy(v023_clear_structured_container_error::run),
    ),
    (
        24,
        "backfill_detect_as",
        MigrationAction::Legacy(v024_backfill_detect_as::run),
    ),
    (
        25,
        "reenable_confirm_delete",
        MigrationAction::Legacy(v025_reenable_confirm_delete::run),
    ),
    (
        26,
        "repoint_acp_default_agent",
        MigrationAction::Legacy(v026_repoint_acp_default_agent::run),
    ),
    (
        27,
        "isolate_sandbox_stores",
        MigrationAction::Legacy(v027_isolate_sandbox_stores::run),
    ),
    (
        28,
        "clear_archived_live_status",
        MigrationAction::Legacy(v028_clear_archived_live_status::run),
    ),
    (
        29,
        "fold_pending_initial_turn",
        MigrationAction::Legacy(v029_fold_pending_initial_turn::run),
    ),
    (
        30,
        "global_only_profile_settings",
        MigrationAction::Legacy(v030_global_only_profile_settings::run),
    ),
    (
        31,
        "conversation_provenance",
        MigrationAction::Legacy(v031_conversation_provenance::run),
    ),
    (
        32,
        "bound_capture_exclusions",
        MigrationAction::Legacy(v032_bound_capture_exclusions::run),
    ),
    (
        33,
        "isolate_sandbox_content",
        MigrationAction::Legacy(v033_isolate_sandbox_content::run),
    ),
    (
        34,
        "trash_retention_minutes",
        MigrationAction::Legacy(v034_trash_retention_minutes::run),
    ),
    (
        35,
        "custom_sort_order",
        MigrationAction::Legacy(v035_custom_sort_order::run),
    ),
    (
        42,
        "canonical_execution_journal",
        MigrationAction::Anchored(v042_canonical_execution_journal::run),
    ),
];

/// The data-schema version this build targets, i.e. the version every install
/// converges to after a successful startup (migration failures abort boot, so a
/// running install is always at this version). Surfaced in telemetry as a coarse
/// version-health signal; see `crate::telemetry`.
pub fn current_schema_version() -> u32 {
    CURRENT_VERSION
}

/// Check whether there are any pending migrations to run.
pub fn has_pending_migrations() -> Result<bool> {
    Ok(get_current_version()? < CURRENT_VERSION)
}

/// Move this session's sandbox store into the private layout, if it is still
/// on the shared one. Called from the container path so the copy is paid by
/// the session that needs it rather than by every pending row on any `aoe`
/// start.
///
/// `reporter` is how a caller with a screen narrates the copy: the TUI
/// forwards it to its status line from a worker thread. Callers without one
/// pass [`progress::tracing_reporter`], which leaves a trail in the log.
///
/// Unproven native content is never a launch fallback: errors leave the store
/// pending, and admission refuses it until a stopped-store transition succeeds.
pub fn migrate_sandbox_store_for_with(
    id: &str,
    reporter: Option<progress::Reporter>,
) -> Result<()> {
    if get_current_version()? < 27 {
        return Ok(());
    }
    let _installed = progress::install(reporter);
    v027_isolate_sandbox_stores::migrate_instance(id)?;
    v033_isolate_sandbox_content::migrate_instance(id)
}

pub(crate) fn migrate_sandbox_store_under_workspace_locks(
    id: &str,
    reporter: Option<progress::Reporter>,
) -> Result<()> {
    if get_current_version()? < 27 {
        return Ok(());
    }
    let _installed = progress::install(reporter);
    v027_isolate_sandbox_stores::migrate_instance_under_workspace_locks(id)?;
    v033_isolate_sandbox_content::migrate_instance_under_workspace_locks(id)
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
    v027_isolate_sandbox_stores::migrate_instance_with(id, is_running, reap)
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
    let app = crate::session::AnchoredDir::open(&crate::session::get_app_dir()?)?;
    let original_app = app.birth_identity()?;
    let validate_app = || {
        anyhow::ensure!(
            crate::session::AnchoredDir::open(app.path())?.birth_identity()? == original_app,
            "original migration app directory changed"
        );
        Ok(())
    };
    let current = schema::read_at(&app)?;
    crate::session::retained_intents::validate_at(&app)?;
    debug!("Current schema version: {}", current);

    if current > CURRENT_VERSION {
        anyhow::bail!(
            "data schema version {current} is newer than this build supports ({CURRENT_VERSION}); refusing to downgrade"
        );
    }
    if current == CURRENT_VERSION {
        validate_app()?;
        anyhow::ensure!(
            schema::read_at(&app)? == current,
            "migration schema changed"
        );
        app.sync()?;
        validate_app()?;
        anyhow::ensure!(
            schema::read_at(&app)? == current,
            "migration schema changed"
        );
        v027_isolate_sandbox_stores::reconcile_pending(announce)?;
        return v033_isolate_sandbox_content::reconcile_pending(announce);
    }

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
        match run {
            MigrationAction::Legacy(run) => {
                run()?;
                set_version_at(&app, *version, Some(&validate_app))?;
            }
            MigrationAction::Anchored(run) => run(&app, *version)?,
        }
        progress::report(progress::Event::Finished {
            version: *version,
            elapsed: start.elapsed(),
        });
        info!(
            target: "migrations",
            version,
            name,
            duration_ms = start.elapsed().as_millis() as u64,
            "migration completed"
        );
    }

    Ok(())
}

fn get_current_version() -> Result<u32> {
    schema::read_at(&crate::session::AnchoredDir::open(
        &crate::session::get_app_dir()?,
    )?)
}

fn set_version_at(
    app: &crate::session::AnchoredDir,
    version: u32,
    validate: Option<&dyn Fn() -> Result<()>>,
) -> Result<()> {
    let bytes = version.to_string();
    app.publish_file(
        std::path::Path::new(VERSION_FILE),
        &mut bytes.as_bytes(),
        std::os::unix::fs::PermissionsExt::from_mode(0o600),
        true,
        validate.map(|validate| crate::session::anchored_fs::FilePublication {
            staging: app,
            validate,
        }),
    )?;
    app.sync()?;
    debug!(version, "updated schema version");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[serial_test::serial]
    fn current_schema_retry_requires_original_directory_durability() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let _environment = crate::session::test_support::isolate_app_dir_at(temp.path());
        let root = crate::session::get_app_dir()?;
        crate::session::retained_intents::initialize_legacy_in(&root)?;
        let marker = CURRENT_VERSION.to_string();
        let rows = br#"[{"id":"retry-owner","opaque":1e400}]"#;
        fs::write(root.join(VERSION_FILE), &marker)?;
        fs::write(root.join("sessions.json"), rows)?;
        let ledger = fs::read(root.join("retained-intents.json"))?;
        let original = crate::session::AnchoredDir::open(&root)?;
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::session::anchored_fs::FAIL_SYNC_IDENTITY_ONCE.set(None);
            }
        }
        let _reset = Reset;
        crate::session::anchored_fs::FAIL_SYNC_IDENTITY_ONCE.set(Some(original.identity()?));
        assert!(run_migrations().is_err());
        assert_eq!(fs::read(root.join(VERSION_FILE))?, marker.as_bytes());
        assert_eq!(fs::read(root.join("sessions.json"))?, rows);
        assert_eq!(fs::read(root.join("retained-intents.json"))?, ledger);
        run_migrations_announced(None)?;
        assert_eq!(fs::read(root.join(VERSION_FILE))?, marker.as_bytes());
        assert_eq!(fs::read(root.join("sessions.json"))?, rows);
        assert_eq!(fs::read(root.join("retained-intents.json"))?, ledger);
        assert!(crate::session::migration_backups(&root.join("sessions.json"))?.is_empty());
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn corrupt_schema_or_retained_ledger_refuses_before_legacy_row_mutation() -> Result<()> {
        for (marker, ledger) in [
            (&b"not-a-version"[..], None),
            (&b"\xff"[..], None),
            (&b"4294967296"[..], None),
            (&b"41"[..], None),
            (&b"22"[..], Some(&b"null"[..])),
        ] {
            let temp = tempfile::tempdir()?;
            let _environment = crate::session::test_support::isolate_app_dir_at(temp.path());
            let root = crate::session::get_app_dir()?;
            let original = br#"[{"id":"retained-original","archived":true,"status":"Waiting","opaque":1e400}]"#;
            fs::write(root.join(VERSION_FILE), marker)?;
            fs::write(root.join("sessions.json"), original)?;
            if let Some(ledger) = ledger {
                fs::write(root.join("retained-intents.json"), ledger)?;
            }
            assert!(run_migrations().is_err());
            assert_eq!(fs::read(root.join(VERSION_FILE))?, marker);
            assert_eq!(fs::read(root.join("sessions.json"))?, original);
            assert!(crate::session::migration_backups(&root.join("sessions.json"))?.is_empty());
        }
        Ok(())
    }

    #[test]
    #[serial_test::serial]
    fn selected_app_dir_refuses_a_newer_schema() {
        let temp = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(temp.path());
        let app = crate::session::get_app_dir().unwrap();
        fs::create_dir_all(&app).unwrap();
        let marker = (CURRENT_VERSION + 1).to_string();
        fs::write(app.join(VERSION_FILE), &marker).unwrap();
        let rows = b"[{\"id\":\"future-owner\",\"runner_journal\":{\"future-evidence\":true}}]";
        fs::write(app.join("sessions.json"), rows).unwrap();
        assert!(run_migrations().is_err());
        assert_eq!(fs::read(app.join(VERSION_FILE)).unwrap(), marker.as_bytes());
        assert_eq!(fs::read(app.join("sessions.json")).unwrap(), rows);
    }

    /// v035 is a schema step and nothing more: an install on the previous version with a
    /// non-default sort order comes out on a newer version with `state.toml` byte for byte
    /// as it was, and running the migrations again changes neither.
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
        let advanced = get_current_version().unwrap();
        assert!(advanced > 34, "the version advances past 34, to {advanced}");
        assert_eq!(advanced, CURRENT_VERSION);
        assert_eq!(
            fs::read_to_string(app.join("state.toml")).unwrap(),
            state,
            "the state is not rewritten"
        );

        run_migrations().unwrap();
        assert_eq!(
            get_current_version().unwrap(),
            advanced,
            "a second run stays put"
        );
        assert_eq!(
            fs::read_to_string(app.join("state.toml")).unwrap(),
            state,
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
            r#"[{"id":"legacy-content","title":"Legacy content","project_path":"/tmp/legacy-content","created_at":"2020-01-01T00:00:00Z","agent_session_id":"old","resume_intent":{"kind":"Use","value":"target"},"retroactive_capture_excludes":["old"]}]"#,
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
        assert_eq!(get_current_version().unwrap(), CURRENT_VERSION);
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
            r#"[{"id":"legacy-backup","title":"Legacy backup","project_path":"/tmp/legacy-backup","created_at":"2020-01-01T00:00:00Z","retroactive_capture_excludes":["legacy-sid"],"pending_initial_turn":"go"}]"#,
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
}
