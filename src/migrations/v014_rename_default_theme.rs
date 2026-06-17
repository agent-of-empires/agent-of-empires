//! Migration v014: rename the `default` builtin theme to `zinc`.
//!
//! The neutral zinc + amber builtin was named `default`, which was ambiguous
//! (it read as "no theme chosen" rather than a specific look). It is now named
//! `zinc`. Theme is a single global preference (see v013), so this only has to
//! rewrite the global `config.toml`: if `[theme].name` is the literal
//! `"default"`, set it to `"zinc"`. Users on the empty default are unaffected
//! (empty still resolves to the fallback, which is now `zinc`). Idempotent: a
//! config that doesn't pin `default` is left untouched.

use anyhow::{Context, Result};
use std::fs;
use std::path::Path;
use tracing::info;

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    rename_theme(&app_dir.join("config.toml"))
}

fn rename_theme(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(path)?;
    let mut doc: toml::Table = content
        .parse()
        .with_context(|| format!("Failed to parse {} during v014 migration", path.display()))?;

    let Some(theme) = doc.get_mut("theme").and_then(|t| t.as_table_mut()) else {
        return Ok(());
    };
    if theme.get("name").and_then(|v| v.as_str()) != Some("default") {
        return Ok(());
    }
    theme.insert("name".into(), toml::Value::String("zinc".into()));

    info!("Renaming theme 'default' -> 'zinc' in {}", path.display());
    crate::session::atomic_write(path, toml::to_string_pretty(&doc)?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(content: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn renames_default_to_zinc() {
        let (_dir, path) = write(
            r#"
[theme]
name = "default"
idle_decay_minutes = 5
"#,
        );
        rename_theme(&path).unwrap();
        let result: toml::Table = fs::read_to_string(&path).unwrap().parse().unwrap();
        let theme = result.get("theme").and_then(|t| t.as_table()).unwrap();
        assert_eq!(theme.get("name").and_then(|v| v.as_str()), Some("zinc"));
        assert_eq!(
            theme.get("idle_decay_minutes").and_then(|v| v.as_integer()),
            Some(5),
            "unrelated keys are preserved"
        );
    }

    #[test]
    fn leaves_other_themes_untouched() {
        let (_dir, path) = write(
            r#"
[theme]
name = "empire"
"#,
        );
        let before = fs::read_to_string(&path).unwrap();
        rename_theme(&path).unwrap();
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn idempotent_when_no_theme_pinned() {
        let (_dir, path) = write(
            r#"
[session]
default_tool = "claude"
"#,
        );
        let before = fs::read_to_string(&path).unwrap();
        rename_theme(&path).unwrap();
        assert_eq!(before, fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn missing_file_is_a_noop() {
        let dir = tempfile::TempDir::new().unwrap();
        assert!(rename_theme(&dir.path().join("nope.toml")).is_ok());
    }
}
