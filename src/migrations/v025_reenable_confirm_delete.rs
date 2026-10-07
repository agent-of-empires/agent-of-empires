//! Migration v025: rewrite a persisted `session.confirm_delete = false` to
//! `true` in the global `config.toml`.
//!
//! The setting shipped defaulting off and `update_config` re-serializes every
//! key, so any install that ever saved a setting carries an explicit `false`
//! that outranks the new compiled default. Nearly all of those are serialized
//! defaults rather than opt-outs, and the rare opt-out is one toggle away.
//! Profile configs are sparse, so a `false` there is a real decision and stays.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir.join("config.toml"))
}

/// Inner body so the test can drive the migration end-to-end against a temp
/// file instead of inlining a near-copy of the production logic.
pub(crate) fn run_in(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(session) = doc.get_mut("session").and_then(|s| s.as_table_mut()) else {
            return false;
        };
        // Only a persisted `false` is rewritten: an absent key already resolves
        // to the new default, and a `true` is where this is headed anyway.
        if session.get("confirm_delete").and_then(|v| v.as_bool()) != Some(false) {
            return false;
        }
        session.insert("confirm_delete".into(), true.into());
        info!(
            "v025: re-enabled session.confirm_delete in {}, which had the pre-#3364 default persisted",
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;

    /// A missing or unparsable config is skipped: this migration corrects a
    /// default, so it must never abort startup.
    #[test]
    fn rewrites_only_a_persisted_false_confirm_delete() {
        let unchanged = |toml: &'static str| (Some(toml), Some(toml));
        assert_rewrites(
            "config.toml",
            run_in,
            &[
                (
                    Some("[session]\nconfirm_delete = false\nsnooze_duration_minutes = 45\n\n[theme]\nname = \"rose-pine\"\n"),
                    Some("[session]\nconfirm_delete = true\nsnooze_duration_minutes = 45\n\n[theme]\nname = \"rose-pine\"\n"),
                ),
                // Already on, by an earlier run or a deliberate opt-in.
                unchanged("[session]\nconfirm_delete = true\n"),
                // An absent key already resolves to the new default.
                unchanged("[session]\ndefault_tool = \"claude\"\n"),
                unchanged("[theme]\nname = \"empire\"\n"),
                unchanged("this is not = = toml"),
                (None, None),
            ],
        );
    }
}
