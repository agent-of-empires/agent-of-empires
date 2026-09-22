use anyhow::{Context, Result};
use std::path::Path;

pub fn run() -> Result<()> {
    let app = crate::session::get_app_dir()?;
    migrate_config_file(&app.join("config.toml"))?;
    let profiles = app.join("profiles");
    if profiles.exists() {
        for entry in std::fs::read_dir(profiles)? {
            let entry = entry?;
            if entry.path().is_dir() {
                migrate_config_file(&entry.path().join("config.toml"))?;
            }
        }
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error).with_context(|| format!("Reading {}", path.display())),
    };
    let mut config: toml::Value =
        toml::from_str(&content).with_context(|| format!("Parsing {}", path.display()))?;
    let removed = config
        .get_mut("session")
        .and_then(toml::Value::as_table_mut)
        .and_then(|session| session.remove("daemon_sidebar"));
    if removed.is_some() {
        crate::session::atomic_write(path, toml::to_string_pretty(&config)?.as_bytes())?;
        tracing::info!(path = %path.display(), "Removed session.daemon_sidebar");
    }
    Ok(())
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
            "[session]\ndaemon_sidebar = false\ndefault_tool = 'codex'\n[status_hooks]\nenabled = true\n",
        )
        .unwrap();
        migrate_config_file(&path).unwrap();
        let migrated = std::fs::read_to_string(&path).unwrap();
        let config: toml::Value = toml::from_str(&migrated).unwrap();
        assert!(config["session"].get("daemon_sidebar").is_none());
        assert_eq!(config["session"]["default_tool"].as_str(), Some("codex"));
        assert_eq!(config["status_hooks"]["enabled"].as_bool(), Some(true));
        migrate_config_file(&path).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), migrated);
    }
}
