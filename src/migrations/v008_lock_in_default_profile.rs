//! Migration v008: lock in the implicit "default" profile for existing
//! installs.
//!
//! Before this version the serde default for `Config.default_profile` was
//! `"default"`, so any user who had never explicitly set the field still
//! resolved to a profile named `default`. The same field now defaults to
//! the empty string and resolution falls through to "first sorted profile
//! directory", which can change which profile the TUI opens dialogs and
//! settings against for users whose profile directories sort before
//! `default` (e.g. `alpha`, `client-a`, `_dev`).
//!
//! For those users the change is non-destructive but visible: new-session
//! dialogs, the settings view, theme reloads, and status hooks all seed
//! from a different profile until the user picks one explicitly. This
//! migration eliminates the surprise by writing `default_profile = "default"`
//! into the global config whenever the install has a `profiles/default/`
//! directory and no explicit override. Fresh installs (no `default`
//! directory) are left alone so the PR's new bootstrap to `main` still
//! takes effect.
//!
//! Skip conditions:
//! - `profiles/default/` does not exist: nothing to preserve.
//! - `default_profile` is already set to a non-empty value: user has
//!   chosen explicitly, do not override.
//!
//! Empty-string values are treated the same as missing: pre-PR an empty
//! string fell through to `"default"`, so locking it in matches old
//! behavior.

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

    fn write_profile_default(app_dir: &Path) {
        fs::create_dir_all(app_dir.join("profiles").join("default")).unwrap();
    }

    #[test]
    fn no_op_when_default_profile_dir_absent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.toml");
        fs::write(&path, "[other]\nkey = \"value\"\n").unwrap();

        run_in(temp.path()).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert!(!after.contains("default_profile"));
    }

    #[test]
    fn no_op_when_default_profile_explicitly_set() {
        let temp = tempfile::tempdir().unwrap();
        write_profile_default(temp.path());
        let path = temp.path().join("config.toml");
        fs::write(&path, "default_profile = \"mzai\"\n").unwrap();

        run_in(temp.path()).unwrap();

        let after: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(after["default_profile"].as_str(), Some("mzai"));
    }

    #[test]
    fn writes_default_when_dir_exists_and_field_missing() {
        let temp = tempfile::tempdir().unwrap();
        write_profile_default(temp.path());
        let path = temp.path().join("config.toml");
        fs::write(&path, "[other]\nkey = \"value\"\n").unwrap();

        run_in(temp.path()).unwrap();

        let after: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(after["default_profile"].as_str(), Some("default"));
        assert!(after.contains_key("other"), "other sections preserved");
    }

    #[test]
    fn writes_default_when_field_is_explicit_empty_string() {
        // Empty string fell through to "default" pre-PR, so locking it in
        // matches the old runtime behavior rather than the user's literal
        // (and surprising) explicit empty value.
        let temp = tempfile::tempdir().unwrap();
        write_profile_default(temp.path());
        let path = temp.path().join("config.toml");
        fs::write(&path, "default_profile = \"\"\n").unwrap();

        run_in(temp.path()).unwrap();

        let after: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(after["default_profile"].as_str(), Some("default"));
    }

    #[test]
    fn writes_default_when_config_file_is_absent() {
        let temp = tempfile::tempdir().unwrap();
        write_profile_default(temp.path());
        let path = temp.path().join("config.toml");
        assert!(!path.exists());

        run_in(temp.path()).unwrap();

        let after: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(after["default_profile"].as_str(), Some("default"));
    }

    #[test]
    fn is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        write_profile_default(temp.path());
        let path = temp.path().join("config.toml");
        fs::write(&path, "[other]\nkey = \"value\"\n").unwrap();

        run_in(temp.path()).unwrap();
        let after_first = fs::read_to_string(&path).unwrap();

        run_in(temp.path()).unwrap();
        let after_second = fs::read_to_string(&path).unwrap();

        assert_eq!(after_first, after_second);
    }

    #[test]
    fn skips_malformed_config_without_failing() {
        let temp = tempfile::tempdir().unwrap();
        write_profile_default(temp.path());
        let path = temp.path().join("config.toml");
        fs::write(&path, "not = valid = toml = at = all\n").unwrap();

        // Migration must not propagate a parse error; downstream code already
        // logs a warning and falls back to defaults on its own.
        run_in(temp.path()).unwrap();

        let after = fs::read_to_string(&path).unwrap();
        assert_eq!(after, "not = valid = toml = at = all\n");
    }
}
