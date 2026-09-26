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
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn moves_session_acp_defaults_under_acp() {
        let untouched = "[session]\nsmart_rename = true\n";
        assert_rewrites(
            "config.toml",
            migrate_config_file,
            &[
                (
                    Some(
                        "[session]\nsmart_rename = true\n\n[session.acp_defaults.opencode]\n\
                         model = \"openai/gpt-5.5\"\neffort = \"high\"\n",
                    ),
                    Some(
                        "[session]\nsmart_rename = true\n\n[acp.acp_defaults.opencode]\n\
                         model = \"openai/gpt-5.5\"\neffort = \"high\"\n",
                    ),
                ),
                // An existing [acp] copy wins over the stale session one.
                (
                    Some(
                        "[session.acp_defaults.opencode]\nmodel = \"stale\"\n\n\
                         [acp.acp_defaults.opencode]\nmodel = \"fresh\"\n",
                    ),
                    Some("[session]\n\n[acp.acp_defaults.opencode]\nmodel = \"fresh\"\n"),
                ),
                (Some(untouched), Some(untouched)),
            ],
        );
    }
}
