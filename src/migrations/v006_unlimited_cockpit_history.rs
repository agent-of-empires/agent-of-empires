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

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let global_config = app_dir.join("config.toml");
    config_file::rewrite(&global_config, |doc| {
        let Some(toml::Value::Table(cockpit)) = doc.get_mut("cockpit") else {
            return false;
        };
        // Only the v005 seed is flipped; any other value is a user's own.
        if cockpit.get("replay_events") != Some(&toml::Value::Integer(500)) {
            return false;
        }
        cockpit.insert("replay_events".into(), (0_i64).into());
        info!(
            "v006: flipped cockpit.replay_events from 500 to 0 (unlimited) in {}",
            global_config.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn rewrites_only_the_default_seed_value() {
        let unchanged = |toml: &'static str| (Some(toml), Some(toml));
        assert_rewrites(
            "config.toml",
            |path| run_in(path.parent().unwrap()),
            &[
                (
                    Some("[cockpit]\nreplay_events = 500\n"),
                    Some("[cockpit]\nreplay_events = 0\n"),
                ),
                unchanged("[cockpit]\nreplay_events = 1000\n"),
                unchanged("[cockpit]\nreplay_events = 0\n"),
                unchanged("[other]\nkey = \"value\"\n"),
            ],
        );
    }
}
