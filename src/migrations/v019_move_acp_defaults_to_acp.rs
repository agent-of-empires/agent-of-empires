//! Migration v019: move `acp_defaults` from `[session]` to `[acp]` in the
//! global config and every profile config; the new field would otherwise
//! silently ignore a configured value. A value already under `[acp]` wins and
//! the stale `[session]` copy is dropped.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    for path in config_file::all_configs(app_dir)? {
        migrate_config_file(&path)?;
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(moved) = doc
            .get_mut("session")
            .and_then(toml::Value::as_table_mut)
            .and_then(|session| session.remove("acp_defaults"))
        else {
            return false;
        };
        // An existing `[acp].acp_defaults` (a prior run, or a manual edit)
        // outranks the stale `[session]` copy.
        if let Some(acp) = doc
            .entry("acp".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
        {
            acp.entry("acp_defaults".to_string()).or_insert(moved);
        }
        info!(
            "v019: moved acp_defaults from [session] to [acp] in {}",
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn moves_acp_defaults_and_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[session]\nsmart_rename = true\n\n[session.acp_defaults.opencode]\nmodel = \"openai/gpt-5.5\"\neffort = \"high\"\n",
        )
        .unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        // Moved under [acp], removed from [session], other session keys kept.
        let acp = doc["acp"].as_table().unwrap();
        let defaults = acp["acp_defaults"].as_table().unwrap();
        assert_eq!(
            defaults["opencode"].as_table().unwrap()["model"].as_str(),
            Some("openai/gpt-5.5")
        );
        assert!(!doc["session"]
            .as_table()
            .unwrap()
            .contains_key("acp_defaults"));
        assert_eq!(
            doc["session"].as_table().unwrap()["smart_rename"].as_bool(),
            Some(true)
        );

        // Idempotent: a second run leaves the file unchanged.
        let before = fs::read_to_string(&path).unwrap();
        migrate_config_file(&path).unwrap();
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn no_session_acp_defaults_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[session]\nsmart_rename = true\n").unwrap();
        let before = fs::read_to_string(&path).unwrap();
        migrate_config_file(&path).unwrap();
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn existing_acp_copy_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[session.acp_defaults.opencode]\nmodel = \"stale\"\n\n[acp.acp_defaults.opencode]\nmodel = \"fresh\"\n",
        )
        .unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let defaults = doc["acp"].as_table().unwrap()["acp_defaults"]
            .as_table()
            .unwrap();
        assert_eq!(
            defaults["opencode"].as_table().unwrap()["model"].as_str(),
            Some("fresh")
        );
        assert!(!doc["session"]
            .as_table()
            .unwrap()
            .contains_key("acp_defaults"));
    }
}
