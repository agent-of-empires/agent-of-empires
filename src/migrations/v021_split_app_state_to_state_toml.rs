//! Migration v021: move `[app_state]` out of `config.toml` into a sibling
//! `state.toml`.
//!
//! `app_state` is global-only runtime/UI bookkeeping (welcome/tour seen,
//! last browse dir, tips seen, sort order, ...), not a settings-schema
//! section. Keeping it inside `config.toml` put its high write churn (every
//! sidebar toggle, every tip dismissal) on the same lock as real settings
//! changes, so a UI toggle and a settings save contended for the same file.
//! Splitting it into its own file, with its own lock, removes that churn from
//! the settings path entirely (see `session::config::update_config` /
//! `session::config::update_app_state`). `state.toml` still gets the same
//! serialised read-modify-write guarantee as `config.toml`, via
//! `storage::locked_update`, so two `aoe` processes never lose an update.
//!
//! Global app dir only: unlike v020's per-profile setting, `app_state` has
//! always been global-only (never merged from a profile override), so there
//! is no `profiles/*/config.toml` to walk.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    let config_path = app_dir.join("config.toml");
    if !config_path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(&config_path)?;
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            warn!("failed to parse {}: {e}, skipping", config_path.display());
            return Ok(());
        }
    };

    let Some(app_state) = doc.remove("app_state") else {
        // Already migrated, or never had one.
        return Ok(());
    };

    let state_path = app_dir.join("state.toml");
    let moved = if state_path.exists() {
        // state.toml already won a previous partial run (or was created by
        // a newer peer); don't clobber it, but still strip the stale key
        // from config.toml below.
        false
    } else {
        crate::session::atomic_write(&state_path, toml::to_string_pretty(&app_state)?.as_bytes())?;
        true
    };

    crate::session::atomic_write(&config_path, toml::to_string_pretty(&doc)?.as_bytes())?;
    info!(
        "v021: removed [app_state] from {}{}",
        config_path.display(),
        if moved {
            format!(" and moved it to {}", state_path.display())
        } else {
            format!(", {} already existed", state_path.display())
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moves_app_state_to_state_toml() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.toml"),
            "default_profile = \"work\"\n\n[app_state]\nhas_seen_welcome = true\n",
        )
        .unwrap();

        run_in(dir.path()).unwrap();

        let config: toml::Table = fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(config["default_profile"].as_str(), Some("work"));
        assert!(!config.contains_key("app_state"));

        let state: toml::Table = fs::read_to_string(dir.path().join("state.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(state["has_seen_welcome"].as_bool(), Some(true));
    }

    #[test]
    fn existing_state_toml_wins_but_key_still_stripped() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.toml"),
            "[app_state]\nhas_seen_welcome = true\n",
        )
        .unwrap();
        fs::write(dir.path().join("state.toml"), "has_seen_welcome = false\n").unwrap();

        run_in(dir.path()).unwrap();

        let config: toml::Table = fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(!config.contains_key("app_state"));

        // The pre-existing state.toml is authoritative; the migration must
        // not overwrite it with the value it found in config.toml.
        let state: toml::Table = fs::read_to_string(dir.path().join("state.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(state["has_seen_welcome"].as_bool(), Some(false));
    }

    #[test]
    fn noop_when_no_app_state_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.toml"),
            "default_profile = \"work\"\n",
        )
        .unwrap();

        run_in(dir.path()).unwrap();

        assert!(!dir.path().join("state.toml").exists());
        let config: toml::Table = fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(config["default_profile"].as_str(), Some("work"));
    }

    #[test]
    fn noop_when_config_toml_missing() {
        let dir = tempfile::tempdir().unwrap();
        run_in(dir.path()).unwrap();
        assert!(!dir.path().join("state.toml").exists());
    }

    #[test]
    fn soft_fails_on_unparseable_config() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.toml"), "not valid toml [[[").unwrap();

        // Must not error; boot should never be blocked by a malformed config.
        run_in(dir.path()).unwrap();

        assert!(!dir.path().join("state.toml").exists());
    }
}
