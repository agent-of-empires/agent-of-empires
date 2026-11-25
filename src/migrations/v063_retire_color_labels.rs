//! Migration v063: drop the retired color labels. Session rows lose `color`,
//! every `config.toml` loses `[session] show_session_colors`, and the
//! dashboard's mirrored repo alias and color store leaves `state.toml`, which
//! would otherwise be hydrated back into every browser. All are ignored on
//! load, so this only stops dead keys from lingering.

use super::{config_file, sessions_file};
use anyhow::Result;
use std::fs;
use std::path::Path;
use tracing::{info, warn};

const REPO_APPEARANCE_KEY: &str = "aoe-repo-appearance-v1";

pub fn run() -> Result<()> {
    run_in(&crate::session::get_app_dir()?)?;
    drop_repo_appearance(&crate::session::config::state_path()?)
}

fn run_in(app_dir: &Path) -> Result<()> {
    for path in sessions_file::session_files(app_dir)? {
        drop_row_colors(&path)?;
    }
    for path in config_file::all_configs(app_dir)? {
        drop_color_setting(&path)?;
    }
    Ok(())
}

fn drop_row_colors(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let _flock = crate::session::acquire_storage_flock(
        path.parent().expect("session file has a parent"),
        crate::session::STORAGE_LOCK_FILENAME,
    )?;
    // An unreadable component keeps its bytes for repair rather than aborting boot.
    let Ok(content) = String::from_utf8(fs::read(path)?) else {
        warn!(target: "migrations.v063", path = %path.display(), "Preserved unreadable session component; its color labels are ignored once it is repaired");
        return Ok(());
    };
    let dropped = sessions_file::heal_rows(path, &content, |row| row.remove("color").is_some())?;
    if dropped > 0 {
        info!(target: "migrations.v063", dropped, path = %path.display(), "dropped session color labels");
    }
    Ok(())
}

/// Edits `state.toml` as plain TOML so an unrelated invalid value cannot block the
/// upgrade; a file that is not TOML keeps its bytes.
fn drop_repo_appearance(path: &Path) -> Result<()> {
    let outcome = crate::session::LockedDataFile::open(path)?.update(
        |content| match content.parse::<toml::Table>() {
            Ok(table) => Ok(Some(table)),
            Err(error) => {
                warn!(target: "migrations.v063", path = %path.display(), %error, "Preserved unparseable state.toml; its repo appearance entry is ignored");
                Ok(None)
            }
        },
        |state: &Option<toml::Table>| match state {
            Some(table) => Ok(toml::to_string_pretty(table)?),
            None => Ok(String::new()),
        },
        |state| -> std::result::Result<bool, std::convert::Infallible> {
            Ok(state
                .as_mut()
                .and_then(|table| table.get_mut("web_ui_state"))
                .and_then(toml::Value::as_table_mut)
                .is_some_and(|ui| ui.remove(REPO_APPEARANCE_KEY).is_some()))
        },
    )?;
    if let Ok((true, _)) = outcome {
        info!(target: "migrations.v063", "dropped web UI repo appearance state");
    }
    Ok(())
}

fn drop_color_setting(path: &Path) -> Result<()> {
    config_file::rewrite(path, |doc| {
        let Some(session) = doc.get_mut("session").and_then(toml::Value::as_table_mut) else {
            return false;
        };
        if session.remove("show_session_colors").is_none() {
            return false;
        }
        if session.is_empty() {
            doc.remove("session");
        }
        info!(target: "migrations.v063", path = %path.display(), "dropped session.show_session_colors");
        true
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::test_cases::assert_rewrites;
    use crate::session::config::{update_app_state, AppStateConfig};

    #[test]
    fn drops_color_from_rows() {
        let kept = r#"[{"id":"b","title":"plain"}]"#;
        assert_rewrites(
            "sessions.json",
            drop_row_colors,
            &[
                (
                    Some(r#"[{"id":"a","color":"red"},{"id":"b","title":"plain"}]"#),
                    Some(r#"[{"id":"a"},{"id":"b","title":"plain"}]"#),
                ),
                (Some(kept), Some(kept)),
                (Some("not json"), Some("not json")),
                (None, None),
            ],
        );
    }

    #[test]
    fn drops_show_session_colors_from_config() {
        let kept = "[session]\nunread_indicator = false\n";
        assert_rewrites(
            "config.toml",
            drop_color_setting,
            &[
                (
                    Some("[session]\nshow_session_colors = false\nunread_indicator = false\n"),
                    Some(kept),
                ),
                (
                    Some("[session]\nshow_session_colors = true\n\n[web]\nmcp_enabled = true\n"),
                    Some("[web]\nmcp_enabled = true\n"),
                ),
                (Some(kept), Some(kept)),
                (None, None),
            ],
        );
    }

    #[test]
    fn repo_appearance_cleanup_tolerates_a_damaged_state_file() {
        let invalid = "has_seen_welcome = \"true\"\n\n[web_ui_state]\naoe-repo-appearance-v1 = \"{}\"\nkept = \"1\"\n";
        assert_rewrites(
            "state.toml",
            drop_repo_appearance,
            &[
                (
                    Some(invalid),
                    Some("has_seen_welcome = \"true\"\n\n[web_ui_state]\nkept = \"1\"\n"),
                ),
                (Some("not = [toml"), Some("not = [toml")),
                (None, None),
            ],
        );
    }

    #[test]
    #[serial_test::serial]
    fn run_covers_profiles_global_config_and_web_ui_state() {
        let dir = tempfile::tempdir().unwrap();
        let _guard = crate::session::test_support::isolate_app_dir_at(dir.path());
        let app = crate::session::get_app_dir().unwrap();
        let profile = app.join("profiles/work");
        fs::create_dir_all(&profile).unwrap();
        fs::write(
            profile.join("sessions.json"),
            r#"[{"id":"a","color":"green"}]"#,
        )
        .unwrap();
        let setting = "[session]\nshow_session_colors = false\n";
        fs::write(profile.join("config.toml"), setting).unwrap();
        fs::write(app.join("config.toml"), setting).unwrap();
        update_app_state(|state| {
            for key in [REPO_APPEARANCE_KEY, "aoe-repo-group-order-v1"] {
                state.web_ui_state.insert(key.into(), "{}".into());
            }
        })
        .unwrap();

        for _ in 0..2 {
            run().unwrap();
        }

        let rows: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(profile.join("sessions.json")).unwrap())
                .unwrap();
        assert_eq!(rows, serde_json::json!([{"id": "a"}]));
        for config in [profile.join("config.toml"), app.join("config.toml")] {
            assert!(!fs::read_to_string(config)
                .unwrap()
                .contains("show_session_colors"));
        }
        let ui = AppStateConfig::load().unwrap().web_ui_state;
        assert_eq!(ui.keys().collect::<Vec<_>>(), ["aoe-repo-group-order-v1"]);
    }
}
