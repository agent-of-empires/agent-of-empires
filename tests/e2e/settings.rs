//! E2E coverage for the Settings TUI.

use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// Save each sidebar position through Settings and verify it in a fresh TUI process.
#[test]
#[parallel]
fn settings_changes_sidebar_position_and_persists_it() {
    require_tmux!();

    let mut h = TuiTestHarness::new("settings_sidebar_position");
    h.spawn_tui();
    h.wait_for_ready();
    let title_column = |screen: &str| {
        let top = screen.lines().next().expect("home title row");
        top.find(" aoe ").expect("list title")
    };
    let left_column = title_column(&h.capture_screen());

    for (value, on_right) in [("right", true), ("left", false)] {
        h.send_keys("s");
        h.wait_for("Settings");
        h.send_keys("/");
        h.type_text("Sidebar Position");
        h.wait_for("Sidebar Position");
        h.send_keys("Enter");
        h.send_keys("Enter");
        h.send_keys("C-s");
        let config = std::fs::read_to_string(app_dir_in(h.home_path()).join("config.toml"))
            .expect("saved config");
        let config: toml::Value = toml::from_str(&config).unwrap();
        assert_eq!(config["session"]["sidebar_position"].as_str(), Some(value));
        h.send_keys("Escape");
        h.send_keys("Escape");
        h.wait_for_ready();
        let column = title_column(&h.capture_screen());
        assert_eq!(column > left_column, on_right);
        if !on_right {
            assert_eq!(column, left_column);
        }

        h.send_keys("q");
        h.wait_for("Quit Agent of Empires");
        h.send_keys("y");
        h.wait_for_exit(std::time::Duration::from_secs(10));
        h.spawn_tui();
        h.wait_for_ready();
        assert_eq!(title_column(&h.capture_screen()), column);
    }
}

#[test]
#[parallel]
fn settings_migration_applies_poller_limit_on_the_first_tui_start() {
    use std::{fs, time::Duration};

    require_tmux!();
    let mut h = TuiTestHarness::new("settings_migration_poller_limit");
    h.set_env("AOE_LOG_LEVEL", "debug");
    let app = app_dir_in(h.home_path());
    let mut config: toml::Table = fs::read_to_string(app.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    let state = config.remove("app_state").unwrap();
    fs::write(app.join("state.toml"), toml::to_string(&state).unwrap()).unwrap();
    config.insert(
        "default_profile".into(),
        toml::Value::String("default".into()),
    );
    fs::write(app.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
    fs::write(app.join(".schema_version"), "29").unwrap();
    fs::write(
        app.join("profiles/default/config.toml"),
        "[session]\nsession_id_poller_max_threads = 1\n",
    )
    .unwrap();
    let project = h.project_path();
    let rows: Vec<_> = [("upgrade1", "Upgrade A"), ("upgrade2", "Upgrade B")]
        .into_iter()
        .map(|(id, title)| {
            h.tmux_new_detached(
                &agent_of_empires::tmux::Session::generate_name(id, title),
                "sleep 600",
            );
            serde_json::json!({
                "id": id, "title": title, "project_path": project, "group_path": "",
                "command": "", "tool": "claude", "yolo_mode": false, "status": "idle",
                "created_at": "2026-01-01T00:00:00Z"
            })
        })
        .collect();
    fs::write(h.sessions_path(), serde_json::to_vec(&rows).unwrap()).unwrap();

    h.spawn_tui();
    h.wait_for_ready();
    crate::harness::wait_until(Duration::from_secs(10), Duration::from_millis(50), || {
        let log = fs::read_to_string(app.join("debug.log")).map_err(|e| e.to_string())?;
        log.contains("budget exhausted; 1/1 threads")
            .then_some(())
            .ok_or_else(|| {
                "the two live sessions have not competed for the migrated one-thread budget".into()
            })
    });
    let saved: toml::Table = fs::read_to_string(app.join("config.toml"))
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(
        saved["session"]["session_id_poller_max_threads"].as_integer(),
        Some(1)
    );
}

/// `aoe settings explain` answers from `config.toml`, so it has to see the
/// migrated file. Asked first on a pre-v009 install it used to read the
/// pre-migration one and report the schema default for a value the user did
/// set, with the legacy key listed as unrecognized.
#[test]
#[parallel]
fn settings_explain_runs_pending_migrations_before_reporting_a_value() {
    use std::fs;

    let h = TuiTestHarness::new("settings_explain_migrates");
    let app = app_dir_in(h.home_path());
    fs::write(
        app.join("config.toml"),
        "[updates]\ncheck_enabled = false\n",
    )
    .unwrap();
    fs::write(app.join(".schema_version"), "8").unwrap();

    let output = h.run_cli(&["settings", "explain", "updates.update_check_mode"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "exit={:?}\n{stdout}\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("updates.update_check_mode = \"off\""),
        "expected the persisted value v009 migrates `check_enabled = false` into, got:\n{stdout}"
    );
    assert!(
        stdout.contains("source: user value"),
        "the value must be attributed to config.toml, not to the schema, got:\n{stdout}"
    );
    assert!(
        !stdout.contains("source: schema default"),
        "the schema default is what this command used to answer with:\n{stdout}"
    );
}

/// The other direction: a value a pending migration *corrects*. v026 repoints
/// the seeded `acp.default_agent = "aoe-agent"` at the current default, so
/// asked first on a pre-v026 install `explain` names an agent this build
/// cannot start.
#[test]
#[parallel]
fn settings_explain_does_not_report_a_value_a_pending_migration_would_replace() {
    use std::fs;

    let h = TuiTestHarness::new("settings_explain_v026");
    let app = app_dir_in(h.home_path());
    fs::write(
        app.join("config.toml"),
        "[acp]\ndefault_agent = \"aoe-agent\"\n",
    )
    .unwrap();
    fs::write(app.join(".schema_version"), "25").unwrap();

    let stdout = h.run_cli_ok(&["settings", "explain", "acp.default_agent"]);

    assert!(
        !stdout.contains("\"aoe-agent\""),
        "v026 repoints that agent at every startup; the first command after an \
         upgrade must not report the value it is about to replace:\n{stdout}"
    );
    assert!(
        stdout.contains("acp.default_agent = \"claude-code\""),
        "expected the value the current schema resolves to, got:\n{stdout}"
    );
}
