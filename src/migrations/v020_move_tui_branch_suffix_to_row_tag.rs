//! Migration v020: replace `worktree.show_branch_in_tui` with `session.row_tag`.
//!
//! The old worktree toggle only covered branch text and is now superseded by the
//! broader TUI row suffix selector. If a user explicitly hid branches with the
//! old key, preserve that intent by seeding `session.row_tag = "none"` unless a
//! row tag value already exists. Then drop the stale worktree key so settings
//! expose a single source of truth.

use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{debug, info};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

pub(crate) fn run_in(app_dir: &Path) -> Result<()> {
    migrate_config_file(&app_dir.join("config.toml"))?;

    let profiles_dir = app_dir.join("profiles");
    if profiles_dir.exists() {
        for entry in fs::read_dir(&profiles_dir)? {
            let entry = entry?;
            if entry.path().is_dir() {
                migrate_config_file(&entry.path().join("config.toml"))?;
            }
        }
    }

    Ok(())
}

fn migrate_config_file(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = match content.parse() {
        Ok(table) => table,
        Err(e) => {
            debug!("failed to parse {}: {e}, skipping", path.display());
            return Ok(());
        }
    };

    let removed = {
        let Some(worktree) = doc.get_mut("worktree").and_then(toml::Value::as_table_mut) else {
            return Ok(());
        };
        worktree.remove("show_branch_in_tui")
    };
    let Some(removed) = removed else {
        return Ok(());
    };

    let seeded_none = if removed.as_bool() == Some(false) {
        let session = doc
            .entry("session".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(table) = session.as_table_mut() {
            if table.contains_key("row_tag") {
                false
            } else {
                table.insert(
                    "row_tag".to_string(),
                    toml::Value::String("none".to_string()),
                );
                true
            }
        } else {
            false
        }
    } else {
        false
    };

    crate::session::atomic_write(path, toml::to_string_pretty(&doc)?.as_bytes())?;
    info!(
        "v020: removed worktree.show_branch_in_tui from {}{}",
        path.display(),
        if seeded_none {
            " and seeded session.row_tag = none"
        } else {
            ""
        }
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
