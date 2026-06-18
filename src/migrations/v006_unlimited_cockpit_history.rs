//! Migration v006: Flip the cockpit history retention default from 500
//! to 0 (unlimited) for existing users on upgrade.
//!
//! v005 seeded `cockpit.replay_events = 500` in config.toml when the
//! cockpit feature shipped. With #1065 we're flipping the default to
//! "keep everything"; users coming from a v005-seeded install would
//! otherwise stay capped at 500. This migration rewrites that specific
//! seeded value (500) to 0 so upgraders pick up the new default. Any
//! user who has explicitly set a different cap is left alone; only the
//! exact v005 seed value triggers the rewrite.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let global_config = app_dir.join("config.toml");
    if !global_config.exists() {
        debug!("global config.toml not present, nothing to migrate");
        return Ok(());
    }

    let content = fs::read_to_string(&global_config)?;
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", global_config.display());
            return Ok(());
        }
    };

    let Some(toml::Value::Table(cockpit)) = doc.get_mut("cockpit") else {
        debug!("no [cockpit] section, nothing to migrate");
        return Ok(());
    };

    let Some(toml::Value::Integer(replay_events)) = cockpit.get("replay_events") else {
        debug!("replay_events absent or non-integer, nothing to migrate");
        return Ok(());
    };

    if *replay_events != 500 {
        debug!(
            current = replay_events,
            "replay_events differs from v005 seed value, leaving alone"
        );
        return Ok(());
    }

    cockpit.insert("replay_events".into(), (0_i64).into());

    let serialized = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(&global_config, serialized.as_bytes())?;

    info!(
        "v006: flipped cockpit.replay_events from 500 to 0 (unlimited) in {}",
        global_config.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_default_seed_value() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[cockpit]\nreplay_events = 500\n").unwrap();

        run_in(temp.path()).unwrap();

        let after: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let cockpit = after.get("cockpit").unwrap().as_table().unwrap();
        assert_eq!(
            cockpit.get("replay_events").unwrap().as_integer().unwrap(),
            0
        );
    }

    #[test]
    fn leaves_explicit_user_values_alone() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[cockpit]\nreplay_events = 1000\n").unwrap();

        run_in(temp.path()).unwrap();

        let after: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let cockpit = after.get("cockpit").unwrap().as_table().unwrap();
        assert_eq!(
            cockpit.get("replay_events").unwrap().as_integer().unwrap(),
            1000
        );
    }

    #[test]
    fn is_idempotent_when_already_zero() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[cockpit]\nreplay_events = 0\n").unwrap();

        run_in(temp.path()).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        run_in(temp.path()).unwrap();
        let second = fs::read_to_string(&path).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn noop_when_no_cockpit_section() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[other]\nkey = \"value\"\n").unwrap();
        run_in(temp.path()).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, "[other]\nkey = \"value\"\n");
    }
}
