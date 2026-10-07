//! Migration v021: move `[app_state]` out of `config.toml` into a sibling
//! `state.toml`, so its UI write churn (sidebar toggles, tip dismissals) stops
//! sharing a lock with settings saves. `state.toml` gets the same serialised
//! read-modify-write guarantee via `storage::locked_update`. Global app dir
//! only: `app_state` was never profile-overridable.

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

    /// An existing state.toml is authoritative, and an unparseable config
    /// must never block boot.
    #[test]
    fn moves_app_state_to_state_toml() {
        let cases: [(Option<&str>, Option<&str>, Option<&str>, Option<&str>); 5] = [
            (
                Some("default_profile = \"work\"\n\n[app_state]\nhas_seen_welcome = true\n"),
                None,
                Some("default_profile = \"work\"\n"),
                Some("has_seen_welcome = true\n"),
            ),
            (
                Some("[app_state]\nhas_seen_welcome = true\n"),
                Some("has_seen_welcome = false\n"),
                Some(""),
                Some("has_seen_welcome = false\n"),
            ),
            (
                Some("default_profile = \"work\"\n"),
                None,
                Some("default_profile = \"work\"\n"),
                None,
            ),
            (None, None, None, None),
            (
                Some("not valid toml [[["),
                None,
                Some("not valid toml [[["),
                None,
            ),
        ];
        let read = |path: &Path| {
            fs::read_to_string(path)
                .ok()
                .map(|s| s.parse::<toml::Table>().unwrap_or_default())
        };
        let parse = |s: Option<&str>| s.map(|s| s.parse::<toml::Table>().unwrap_or_default());
        for (config, state, config_after, state_after) in cases {
            let dir = tempfile::tempdir().unwrap();
            let (config_path, state_path) = (
                dir.path().join("config.toml"),
                dir.path().join("state.toml"),
            );
            for (path, content) in [(&config_path, config), (&state_path, state)] {
                if let Some(content) = content {
                    fs::write(path, content).unwrap();
                }
            }
            run_in(dir.path()).unwrap();
            assert_eq!(read(&config_path), parse(config_after), "{config:?}");
            assert_eq!(read(&state_path), parse(state_after), "{config:?}");
        }
    }

    /// A `config.toml` symlinked into a dotfiles repo must survive the
    /// rewrite: the link stays a link and the stripped content lands on the
    /// target, instead of `rename(2)` replacing the link with a regular file
    /// (#3186). v021 stands in for every migration here; they all share the
    /// same `atomic_write` primitive.
    #[cfg(unix)]
    #[test]
    fn symlinked_config_survives_migration() {
        let dir = tempfile::tempdir().unwrap();
        let dotfiles = dir.path().join("dotfiles");
        fs::create_dir_all(&dotfiles).unwrap();

        let target = dotfiles.join("aoe-config.toml");
        fs::write(
            &target,
            "default_profile = \"work\"\n\n[app_state]\nhas_seen_welcome = true\n",
        )
        .unwrap();

        let app_dir = dir.path().join("app");
        fs::create_dir_all(&app_dir).unwrap();
        let link = app_dir.join("config.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        run_in(&app_dir).unwrap();

        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the migration must not replace the symlink with a regular file"
        );
        let config: toml::Table = fs::read_to_string(&target).unwrap().parse().unwrap();
        assert!(
            !config.contains_key("app_state"),
            "the rewrite must land on the symlink target, got: {config:?}"
        );
        assert_eq!(config["default_profile"].as_str(), Some("work"));
    }
}
