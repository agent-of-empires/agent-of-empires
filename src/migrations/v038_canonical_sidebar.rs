use anyhow::{Context, Result};
use std::path::Path;

pub fn run() -> Result<()> {
    let app = crate::session::get_app_dir()?;
    migrate_config_file(&app.join("config.toml"));
    let profiles = app.join("profiles");
    if profiles.exists() {
        for entry in std::fs::read_dir(profiles)? {
            let entry = entry?;
            if entry.path().is_dir() {
                migrate_config_file(&entry.path().join("config.toml"));
            }
        }
    }
    Ok(())
}

/// A config the daemon cannot parse is a live-config problem, not a reason to
/// brick startup: the daemon already falls back to defaults through
/// `Config::load_or_warn`, so an unparseable file is reported and left
/// untouched for the user to repair.
fn migrate_config_file(path: &Path) {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
        Err(error) => {
            tracing::warn!(
                target: "migration.v038",
                path = %path.display(),
                %error,
                "Could not read config for the canonical sidebar migration; skipping"
            );
            return;
        }
    };
    let migration: Result<()> = (|| -> Result<()> {
        let mut config: toml_edit::DocumentMut = content
            .parse()
            .with_context(|| format!("Parsing {}", path.display()))?;
        let removed = config
            .get_mut("session")
            .and_then(toml_edit::Item::as_table_mut)
            .and_then(|session| session.remove("daemon_sidebar"));
        if removed.is_some() {
            crate::session::atomic_write(path, config.to_string().as_bytes())?;
            tracing::info!(path = %path.display(), "Removed session.daemon_sidebar");
        }
        Ok(())
    })();
    if let Err(error) = migration {
        tracing::warn!(
            target: "migration.v038",
            path = %path.display(),
            %error,
            "Skipping the canonical sidebar migration for an unreadable config"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_sidebar_opt_out_preserves_other_settings_and_is_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        std::fs::write(
            &path,
            "# global comment\n[session] # session comment\ndaemon_sidebar = false\ndefault_tool = 'codex' # keep tool comment\n[status_hooks]\nenabled = true # keep hook comment\n",
        )
        .unwrap();
        migrate_config_file(&path);
        let migrated = std::fs::read_to_string(&path).unwrap();
        let config: toml::Value = toml::from_str(&migrated).unwrap();
        assert!(migrated.contains("# global comment"));
        assert!(migrated.contains("[session] # session comment"));
        assert!(migrated.contains("default_tool = 'codex' # keep tool comment"));
        assert!(migrated.contains("enabled = true # keep hook comment"));
        assert!(config["session"].get("daemon_sidebar").is_none());
        assert_eq!(config["session"]["default_tool"].as_str(), Some("codex"));
        assert_eq!(config["status_hooks"]["enabled"].as_bool(), Some(true));
        migrate_config_file(&path);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated);
    }

    #[test]
    fn an_unparseable_config_is_reported_and_left_untouched() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let broken = "[session\ndaemon_sidebar = false\n";
        std::fs::write(&path, broken).unwrap();
        migrate_config_file(&path);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            broken,
            "a config the user must repair is never rewritten by the migration"
        );
    }
}
