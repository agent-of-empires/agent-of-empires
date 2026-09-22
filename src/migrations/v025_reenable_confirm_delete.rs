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
    use std::fs;

    fn write(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    fn confirm_delete_after(content: &str) -> Option<bool> {
        let (_dir, path) = write(content);
        run_in(&path).unwrap();
        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        doc.get("session")?
            .as_table()?
            .get("confirm_delete")?
            .as_bool()
    }

    #[test]
    fn rewrites_only_a_persisted_false() {
        let cases = [
            // The serialized old default: the case this migration exists for.
            ("[session]\nconfirm_delete = false\n", Some(true)),
            // Already on, whether by an earlier run or a deliberate opt-in.
            ("[session]\nconfirm_delete = true\n", Some(true)),
            // An absent key already resolves to the new default; leave it out
            // rather than pinning it.
            ("[session]\ndefault_tool = \"claude\"\n", None),
            // No [session] table at all (a config that only sets a theme).
            ("[theme]\nname = \"empire\"\n", None),
        ];
        for (content, expected) in cases {
            assert_eq!(confirm_delete_after(content), expected, "{content:?}");
        }
    }

    #[test]
    fn preserves_other_settings_and_is_idempotent() {
        let (_dir, path) = write(
            "[session]\nconfirm_delete = false\nsnooze_duration_minutes = 45\n\n\
             [theme]\nname = \"rose-pine\"\n",
        );

        run_in(&path).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        let doc: toml::Table = first.parse().unwrap();
        let session = doc.get("session").unwrap().as_table().unwrap();
        assert_eq!(
            session.get("confirm_delete").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            session
                .get("snooze_duration_minutes")
                .and_then(|v| v.as_integer()),
            Some(45),
            "sibling session settings must survive the rewrite"
        );
        assert_eq!(
            doc.get("theme")
                .and_then(|t| t.as_table())
                .and_then(|t| t.get("name"))
                .and_then(|v| v.as_str()),
            Some("rose-pine"),
            "unrelated sections must survive the rewrite"
        );

        run_in(&path).unwrap();
        assert_eq!(
            first,
            fs::read_to_string(&path).unwrap(),
            "a second run must not rewrite the file"
        );
    }

    /// A missing or unparsable config is skipped, never a startup-aborting
    /// error: this migration corrects a default, so it must not brick boot.
    #[test]
    fn unusable_config_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(run_in(&dir.path().join("nope.toml")).is_ok());

        let (_dir, path) = write("this is not = = toml");
        assert!(run_in(&path).is_ok());
    }
}
