//! Migration v020: replace `worktree.show_branch_in_tui` with `session.row_tag`.
//!
//! The old worktree toggle only covered branch text and is now superseded by the
//! broader TUI row suffix selector. If a user explicitly hid branches with the
//! old key, preserve that intent by seeding `session.row_tag = "none"` unless a
//! row tag value already exists. Then drop the stale worktree key so settings
//! expose a single source of truth.

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
        let removed = doc
            .get_mut("worktree")
            .and_then(toml::Value::as_table_mut)
            .and_then(|worktree| worktree.remove("show_branch_in_tui"));
        let Some(removed) = removed else {
            return false;
        };

        // Only the opt-out carries over, and never over an explicit `row_tag`.
        let mut seeded_none = false;
        if removed.as_bool() == Some(false) {
            if let Some(session) = doc
                .entry("session".to_string())
                .or_insert_with(|| toml::Value::Table(toml::Table::new()))
                .as_table_mut()
            {
                seeded_none = !session.contains_key("row_tag");
                if seeded_none {
                    session.insert(
                        "row_tag".to_string(),
                        toml::Value::String("none".to_string()),
                    );
                }
            }
        }

        info!(
            "v020: removed worktree.show_branch_in_tui from {}{}",
            path.display(),
            if seeded_none {
                " and seeded session.row_tag = none"
            } else {
                ""
            }
        );
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn false_legacy_toggle_seeds_row_tag_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[worktree]\nshow_branch_in_tui = false\nauto_cleanup = true\n",
        )
        .unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["session"]["row_tag"].as_str(), Some("none"));
        assert!(!doc["worktree"]
            .as_table()
            .unwrap()
            .contains_key("show_branch_in_tui"));
        assert_eq!(doc["worktree"]["auto_cleanup"].as_bool(), Some(true));
    }

    #[test]
    fn true_legacy_toggle_only_removes_stale_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[worktree]\nshow_branch_in_tui = true\n").unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert!(!doc["worktree"]
            .as_table()
            .unwrap()
            .contains_key("show_branch_in_tui"));
        assert!(doc.get("session").is_none());
    }

    #[test]
    fn existing_row_tag_wins() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[session]\nrow_tag = \"profile\"\n\n[worktree]\nshow_branch_in_tui = false\n",
        )
        .unwrap();

        migrate_config_file(&path).unwrap();

        let doc: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        assert_eq!(doc["session"]["row_tag"].as_str(), Some("profile"));
        assert!(!doc["worktree"]
            .as_table()
            .unwrap()
            .contains_key("show_branch_in_tui"));
    }

    #[test]
    fn migrates_profile_configs() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("profiles/default");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("config.toml"),
            "[worktree]\nshow_branch_in_tui = false\n",
        )
        .unwrap();

        run_in(dir.path()).unwrap();

        let doc: toml::Table = fs::read_to_string(profile.join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(doc["session"]["row_tag"].as_str(), Some("none"));
    }
}
