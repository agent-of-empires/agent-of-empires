//! Migration v005: Seed cockpit settings on upgrade.
//!
//! Seed the global `[cockpit]` section with defaults when absent, so users
//! can enable the feature without first running the settings TUI.
//!
//! Per-profile configs are left alone; the merge logic in
//! `profile_config.rs` falls back to the global value when a profile
//! doesn't override.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

/// Inner body so the test can drive the migration end-to-end against
/// a temp directory instead of inlining a near-copy of the production
/// logic that drifts as fields are added/removed.
pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let global_config = app_dir.join("config.toml");
    config_file::rewrite(&global_config, |doc| {
        if doc.contains_key("cockpit") {
            return false;
        }
        let mut cockpit = toml::Table::new();
        cockpit.insert("enabled".into(), false.into());
        cockpit.insert("default_for_claude".into(), true.into());
        cockpit.insert("default_agent".into(), "aoe-agent".into());
        cockpit.insert("max_concurrent_workers".into(), (5_i64).into());
        cockpit.insert("replay_events".into(), (500_i64).into());
        cockpit.insert("replay_bytes".into(), (5_242_880_i64).into());
        doc.insert("cockpit".into(), toml::Value::Table(cockpit));
        info!(
            "v005: seeded [cockpit] section in {}",
            global_config.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn seeding_is_idempotent_and_preserves_other_sections() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[other]\nkey = \"value\"\n").unwrap();

        // First run: seeds [cockpit].
        run_in(temp.path()).unwrap();
        let after_first: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(after_first.contains_key("cockpit"));
        assert!(after_first.contains_key("other"));

        // Second run: idempotent (sees existing [cockpit] and bails).
        run_in(temp.path()).unwrap();
        let after_second = fs::read_to_string(&path).unwrap();
        assert_eq!(
            toml::to_string_pretty(&after_first).unwrap(),
            after_second,
            "second run must not mutate config",
        );
    }
}
