//! Move the mobile terminal font out of opaque web UI state.

use anyhow::{bail, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

pub fn run() -> Result<()> {
    let app_dir = crate::session::get_app_dir()?;
    run_in(&app_dir)
}

fn run_in(app_dir: &Path) -> Result<()> {
    let state_path = app_dir.join("state.toml");
    if !state_path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&state_path)?;
    let mut state: toml::Table = match content.parse() {
        Ok(state) => state,
        Err(error) => {
            warn!(
                "v041: failed to parse {}: {error}, skipping",
                state_path.display()
            );
            return Ok(());
        }
    };
    let Some(settings) = state
        .get_mut("web_ui_state")
        .and_then(toml::Value::as_table_mut)
        .and_then(|web_ui_state| web_ui_state.get_mut("aoe-web-settings"))
        .and_then(|settings| settings.as_str())
    else {
        return Ok(());
    };
    let mut preferences: Value = match serde_json::from_str(settings) {
        Ok(preferences) => preferences,
        Err(error) => {
            warn!("v041: failed to parse aoe-web-settings JSON: {error}, skipping");
            return Ok(());
        }
    };
    let Some(preferences) = preferences.as_object_mut() else {
        return Ok(());
    };
    let Some(legacy) = preferences.remove("mobileFontSize") else {
        return Ok(());
    };

    let legacy_size = legacy
        .as_u64()
        .filter(|size| (6..=28).contains(size))
        .map(|size| size as i64);
    if let Some(size) = legacy_size {
        migrate_config_value(app_dir, size)?;
    }

    let settings = serde_json::to_string(preferences)?;
    state["web_ui_state"]["aoe-web-settings"] = toml::Value::String(settings);
    crate::session::atomic_write(&state_path, toml::to_string_pretty(&state)?.as_bytes())?;
    info!(
        "v041: retired mobileFontSize from aoe-web-settings in {}",
        app_dir.display()
    );
    Ok(())
}

/// Carry the old choice forward once. A value already present in config.toml,
/// including an explicitly configured 14, always takes precedence.
fn migrate_config_value(app_dir: &Path, legacy_size: i64) -> Result<()> {
    let path = app_dir.join("config.toml");
    let mut config: toml::Table = if path.exists() {
        fs::read_to_string(&path)?.parse()?
    } else {
        toml::Table::new()
    };
    let web = config
        .entry("web")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut();
    let Some(web) = web else {
        bail!(
            "v041 cannot migrate mobile_terminal_font_size because [web] in {} is not a table",
            path.display()
        );
    };
    if web.contains_key("mobile_terminal_font_size") {
        return Ok(());
    }
    web.insert(
        "mobile_terminal_font_size".to_owned(),
        toml::Value::Integer(legacy_size),
    );
    crate::session::atomic_write(&path, toml::to_string_pretty(&config)?.as_bytes())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_with(preferences: &str) -> String {
        format!(
            "unrelated = \"keep\"\n\n[web_ui_state]\n\"aoe-web-settings\" = {}\nother = \"also keep\"\n",
            toml::Value::String(preferences.to_owned())
        )
    }

    fn config_value(dir: &Path) -> Option<i64> {
        fs::read_to_string(dir.join("config.toml"))
            .ok()?
            .parse::<toml::Table>()
            .ok()?
            .get("web")?
            .get("mobile_terminal_font_size")?
            .as_integer()
    }

    fn migrated_preferences(dir: &Path) -> String {
        let state: toml::Table = fs::read_to_string(dir.join("state.toml"))
            .unwrap()
            .parse()
            .unwrap();
        state["web_ui_state"]["aoe-web-settings"]
            .as_str()
            .unwrap()
            .to_owned()
    }

    #[test]
    fn migrates_valid_legacy_choice_and_preserves_other_preferences() {
        for size in [8, 14] {
            let dir = tempfile::tempdir().unwrap();
            let original = state_with(&format!(
                r#"{{"mobileFontSize":{size},"desktopFontSize":18,"sidebar":"right"}}"#
            ));
            fs::write(dir.path().join("state.toml"), original).unwrap();

            run_in(dir.path()).unwrap();

            assert_eq!(config_value(dir.path()), Some(size));
            assert_eq!(
                serde_json::from_str::<Value>(&migrated_preferences(dir.path())).unwrap(),
                serde_json::json!({"desktopFontSize": 18, "sidebar": "right"})
            );
            let state: toml::Table = fs::read_to_string(dir.path().join("state.toml"))
                .unwrap()
                .parse()
                .unwrap();
            assert_eq!(state["unrelated"].as_str(), Some("keep"));
            assert_eq!(state["web_ui_state"]["other"].as_str(), Some("also keep"));
            let once = fs::read(dir.path().join("state.toml")).unwrap();
            run_in(dir.path()).unwrap();
            assert_eq!(fs::read(dir.path().join("state.toml")).unwrap(), once);
            assert_eq!(config_value(dir.path()), Some(size));
        }
    }

    #[test]
    fn explicit_config_wins_and_legacy_value_is_retired() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("config.toml"),
            "[web]\nmobile_terminal_font_size = 14\nnotifications_enabled = false\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("state.toml"),
            state_with(r#"{"mobileFontSize":8,"desktopFontSize":18}"#),
        )
        .unwrap();

        run_in(dir.path()).unwrap();

        assert_eq!(config_value(dir.path()), Some(14));
        assert_eq!(
            migrated_preferences(dir.path()),
            r#"{"desktopFontSize":18}"#
        );
        let config: toml::Table = fs::read_to_string(dir.path().join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(
            config["web"]["notifications_enabled"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn invalid_legacy_values_are_removed_without_creating_config() {
        for (legacy, expected) in [
            (
                r#"{"mobileFontSize":4,"desktopFontSize":18}"#,
                r#"{"desktopFontSize":18}"#,
            ),
            (r#"{"mobileFontSize":"14"}"#, "{}"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("state.toml"), state_with(legacy)).unwrap();

            run_in(dir.path()).unwrap();

            assert_eq!(config_value(dir.path()), None);
            assert!(!dir.path().join("config.toml").exists());
            assert_eq!(migrated_preferences(dir.path()), expected);
        }
    }

    #[test]
    fn malformed_state_is_left_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.toml");
        let original = b"[web_ui_state\nopaque = \"bytes\"";
        fs::write(&path, original).unwrap();
        run_in(dir.path()).unwrap();
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn malformed_preferences_are_preserved_and_failed_config_write_keeps_legacy() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.toml");
        for original in [state_with("{broken"), state_with("[1,2]")] {
            fs::write(&state_path, &original).unwrap();
            run_in(dir.path()).unwrap();
            assert_eq!(fs::read_to_string(&state_path).unwrap(), original);
        }
        let original = state_with(r#"{"mobileFontSize":18,"desktopFontSize":16}"#);
        fs::write(&state_path, &original).unwrap();
        fs::write(dir.path().join("config.toml"), "invalid [toml").unwrap();
        assert!(run_in(dir.path()).is_err());
        assert_eq!(fs::read_to_string(&state_path).unwrap(), original);
    }
}
