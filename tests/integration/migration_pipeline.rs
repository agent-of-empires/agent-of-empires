//! Integration tests for the migration pipeline: execution, versioning, and idempotency.

use anyhow::Result;
use serial_test::serial;
use std::fs;

use crate::common::setup_temp_home;

/// A fresh app dir, and one whose version file reads 0, both advance to the
/// current schema version; a second run leaves the file untouched.
#[test]
#[serial]
fn migrations_advance_to_current_version_once() -> Result<()> {
    let current = agent_of_empires::migrations::current_schema_version();
    for seed in [None, Some("0")] {
        let _temp = setup_temp_home();
        let version_path = agent_of_empires::session::get_app_dir()?.join(".schema_version");
        assert!(!version_path.exists());
        if let Some(seed) = seed {
            fs::write(&version_path, seed)?;
        }

        agent_of_empires::migrations::run_migrations()?;
        let after_first = fs::read_to_string(&version_path)?;
        assert_eq!(after_first.trim().parse::<u32>()?, current, "{seed:?}");

        agent_of_empires::migrations::run_migrations()?;
        assert_eq!(fs::read_to_string(&version_path)?, after_first, "{seed:?}");
    }
    Ok(())
}

#[test]
#[serial]
fn post_v029_migrations_run_together_before_version_advances() -> Result<()> {
    let _temp = setup_temp_home();
    let app = agent_of_empires::session::get_app_dir()?;
    let profile = agent_of_empires::session::get_profile_dir("work")?.join("config.toml");
    let sessions = profile.with_file_name("sessions.json");
    fs::write(app.join(".schema_version"), "29")?;
    fs::write(app.join("config.toml"), "default_profile = 'work'\n")?;
    fs::write(&profile, "[invalid")?;

    fs::write(
        &sessions,
        r#"[{"agent_session_id":"legacy","retroactive_capture_excludes":["excluded"]}]"#,
    )?;
    assert!(agent_of_empires::migrations::run_migrations().is_err());
    assert_eq!(fs::read_to_string(app.join(".schema_version"))?, "29");
    assert_eq!(fs::read_to_string(&profile)?, "[invalid");

    fs::write(&profile, "[theme]\nname = 'dracula'\n[session]\nconfirm_before_quit = false\ndefault_tool = 'codex'\n")?;
    agent_of_empires::migrations::run_migrations()?;
    let global = agent_of_empires::session::Config::load()?;
    assert_eq!(global.theme.name, "dracula");
    assert!(!global.session.confirm_before_quit);
    let effective = agent_of_empires::session::resolve_config("work")?;
    assert_eq!(effective.theme.name, "dracula");
    assert!(!effective.session.confirm_before_quit);
    assert_eq!(effective.session.default_tool.as_deref(), Some("codex"));
    let saved: toml::Table = fs::read_to_string(&profile)?.parse()?;
    assert!(!saved.contains_key("theme"));
    let rows: serde_json::Value = serde_json::from_str(&fs::read_to_string(&sessions)?)?;
    assert_eq!(rows[0]["agent_session_binding"]["session_id"], "legacy");
    assert_eq!(
        rows[0]["retroactive_capture_excludes"][0]["session_id"],
        "excluded"
    );
    assert_eq!(saved["session"].as_table().unwrap().len(), 1);
    assert_eq!(
        fs::read_to_string(app.join(".schema_version"))?.parse::<u32>()?,
        agent_of_empires::migrations::current_schema_version()
    );
    Ok(())
}

#[test]
#[serial]
fn schema_v30_runs_pr_conversation_migrations() -> Result<()> {
    let _temp = setup_temp_home();
    let app = agent_of_empires::session::get_app_dir()?;
    let sessions = app.join("sessions.json");
    fs::write(app.join(".schema_version"), "30")?;
    fs::write(
        &sessions,
        r#"[{"agent_session_id":"legacy","retroactive_capture_excludes":["excluded"]}]"#,
    )?;

    agent_of_empires::migrations::run_migrations()?;

    let rows: serde_json::Value = serde_json::from_str(&fs::read_to_string(&sessions)?)?;
    assert_eq!(rows[0]["agent_session_binding"]["session_id"], "legacy");
    assert_eq!(
        rows[0]["retroactive_capture_excludes"][0]["session_id"],
        "excluded"
    );
    assert_eq!(
        fs::read_to_string(app.join(".schema_version"))?.parse::<u32>()?,
        agent_of_empires::migrations::current_schema_version()
    );
    Ok(())
}
