//! Migration v036: drop `session.click_action`. A single click on a session
//! row now only selects it and toggles its subagent rows, so the choice
//! between entering live mode and selecting is gone.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    for path in config_file::all_configs(app_dir)? {
        migrate_config_file(&path)?;
    }
    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(removed) = doc
            .get_mut("session")
            .and_then(toml::Value::as_table_mut)
            .and_then(|session| session.remove("click_action"))
        else {
            return false;
        };
        info!(
            "v036: dropped session.click_action = {removed} from {}",
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
    fn drops_click_action() {
        assert_rewrites(
            "config.toml",
            migrate_config_file,
            &[
                (
                    Some("[session]\nclick_action = \"select_only\"\nconfirm_delete = false\n"),
                    Some("[session]\nconfirm_delete = false\n"),
                ),
                (
                    Some("[session]\nconfirm_delete = false\n"),
                    Some("[session]\nconfirm_delete = false\n"),
                ),
                (None, None),
            ],
        );
    }
}
