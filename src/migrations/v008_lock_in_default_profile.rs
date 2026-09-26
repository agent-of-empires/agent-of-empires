//! Migration v008: write `default_profile = "default"` into the global config
//! for installs that have a `profiles/default/` directory and no explicit
//! choice (empty counts as none).
//!
//! The field no longer defaults to `"default"`; resolution now falls through
//! to the first sorted profile directory, which would silently retarget those
//! installs. A fresh install has no `default` directory and is left alone.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let default_profile_dir = app_dir.join("profiles").join("default");
    if !default_profile_dir.exists() {
        debug!("no profiles/default/ directory, nothing to lock in");
        return Ok(());
    }

    let global_config = app_dir.join("config.toml");
    let content = if global_config.exists() {
        fs::read_to_string(&global_config)?
    } else {
        String::new()
    };

    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", global_config.display());
            return Ok(());
        }
    };

    let already_set = doc
        .get("default_profile")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.is_empty());
    if already_set {
        debug!("default_profile already set explicitly, skipping");
        return Ok(());
    }

    doc.insert(
        "default_profile".into(),
        toml::Value::String("default".into()),
    );

    let serialized = toml::to_string_pretty(&doc)?;
    crate::session::atomic_write(&global_config, serialized.as_bytes())?;

    info!(
        "v008: locked in default_profile = \"default\" in {}",
        global_config.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn locks_in_default_only_when_the_default_profile_dir_exists() {
        let other = "[other]\nkey = \"value\"\n";
        let with_dir = |path: &Path| {
            let app_dir = path.parent().unwrap();
            fs::create_dir_all(app_dir.join("profiles").join("default"))?;
            run_in(app_dir)
        };
        let explicit = "default_profile = \"mzai\"\n";
        // A parse error is left for the config loader to warn about.
        let malformed = "not = valid = toml = at = all\n";
        assert_rewrites(
            "config.toml",
            with_dir,
            &[
                (
                    Some(other),
                    Some("default_profile = \"default\"\n[other]\nkey = \"value\"\n"),
                ),
                // Empty fell through to "default" at runtime before this migration.
                (
                    Some("default_profile = \"\"\n"),
                    Some("default_profile = \"default\"\n"),
                ),
                (None, Some("default_profile = \"default\"\n")),
                (Some(explicit), Some(explicit)),
                (Some(malformed), Some(malformed)),
            ],
        );
        assert_rewrites(
            "config.toml",
            |path| run_in(path.parent().unwrap()),
            &[(Some(other), Some(other))],
        );
    }
}
