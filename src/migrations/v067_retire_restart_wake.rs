//! Migration v067: every `config.toml` loses `[session] restart_wake_message` and
//! `startup_recovery_wake`, now that a restart sends nothing. Both are ignored on load, so this
//! only stops dead keys from lingering.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

const RETIRED: [&str; 2] = ["restart_wake_message", "startup_recovery_wake"];

pub fn run() -> Result<()> {
    for path in config_file::all_configs(&crate::session::get_app_dir()?)? {
        drop_wake_settings(&path)?;
    }
    Ok(())
}

fn drop_wake_settings(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(session) = doc.get_mut("session").and_then(toml::Value::as_table_mut) else {
            return false;
        };
        let dropped = RETIRED.iter().fold(false, |dropped, key| {
            session.remove(*key).is_some() | dropped
        });
        if !dropped {
            return false;
        }
        if session.is_empty() {
            doc.remove("session");
        }
        info!(target: "migrations.v067", path = %path.display(), "dropped the restart wake settings");
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    #[test]
    fn drops_the_wake_settings_from_config() {
        let kept = "[session]\nauto_resume_on_restart = false\n";
        assert_rewrites(
            "config.toml",
            drop_wake_settings,
            &[
                (
                    Some("[session]\nrestart_wake_message = \"go\"\nstartup_recovery_wake = true\nauto_resume_on_restart = false\n"),
                    Some(kept),
                ),
                (
                    Some("[session]\nrestart_wake_message = \"\"\n\n[web]\nmcp_enabled = true\n"),
                    Some("[web]\nmcp_enabled = true\n"),
                ),
                (Some(kept), Some(kept)),
                (None, None),
            ],
        );
    }
}
