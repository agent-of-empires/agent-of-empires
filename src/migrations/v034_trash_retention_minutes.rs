//! Migration v034: replace `session.trash_retention_days` with
//! `session.trash_retention_minutes` so retention can be shorter than a day.
//! A carried value is converted, never over an existing minutes key.

use super::config_file;
use anyhow::Result;
use std::path::Path;
use tracing::info;

const MINUTES_PER_DAY: i64 = 24 * 60;

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
    config_file::rewrite_strict(path, "trash_retention_minutes", |doc| {
        let Some(session) = doc.get_mut("session").and_then(toml::Value::as_table_mut) else {
            return false;
        };
        let Some(days) = session.remove("trash_retention_days") else {
            return false;
        };
        // An out-of-range value never loaded before either; dropping it keeps the default.
        let minutes = days
            .as_integer()
            .filter(|days| (0..=i64::from(u32::MAX) / MINUTES_PER_DAY).contains(days))
            .map(|days| days * MINUTES_PER_DAY);
        let converted = match minutes {
            Some(minutes) if !session.contains_key("trash_retention_minutes") => {
                session.insert("trash_retention_minutes".into(), minutes.into());
                true
            }
            _ => false,
        };
        info!(
            "v034: {} session.trash_retention_days in {}",
            if converted { "converted" } else { "dropped" },
            path.display()
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;
    use std::fs;

    #[test]
    fn converts_days_to_minutes() {
        assert_rewrites(
            "config.toml",
            migrate_config_file,
            &[
                (
                    Some("[session]\ntrash_retention_days = 1\ndelete_to_trash = true\n"),
                    Some("[session]\ntrash_retention_minutes = 1440\ndelete_to_trash = true\n"),
                ),
                // 0 still means keep forever.
                (
                    Some("[session]\ntrash_retention_days = 0\n"),
                    Some("[session]\ntrash_retention_minutes = 0\n"),
                ),
                // An explicit minutes value wins.
                (
                    Some("[session]\ntrash_retention_days = 3\ntrash_retention_minutes = 90\n"),
                    Some("[session]\ntrash_retention_minutes = 90\n"),
                ),
                // A value the old field could not hold is dropped.
                (
                    Some("[session]\ntrash_retention_days = -1\n"),
                    Some("[session]\n"),
                ),
                (
                    Some("[session]\nconfirm_delete = false\n"),
                    Some("[session]\nconfirm_delete = false\n"),
                ),
                (None, None),
            ],
        );
    }

    #[test]
    fn migrates_profile_configs() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/work");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("config.toml"),
            "[session]\ntrash_retention_days = 30\n",
        )
        .unwrap();

        run_in(dir.path()).unwrap();

        let doc: toml::Table = fs::read_to_string(profile.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            doc["session"]["trash_retention_minutes"].as_integer(),
            Some(30 * 1440)
        );
    }
}
