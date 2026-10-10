//! E2E coverage for the Settings TUI.

use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

#[test]
#[parallel]
fn settings_shortcut_bar_hides_the_row_and_survives_restart() {
    require_tmux!();
    let mut h = TuiTestHarness::new("settings_shortcut_bar");
    h.spawn_tui();
    h.wait_for_ready();
    h.assert_screen_contains("Cmds");
    h.send_keys("s");
    h.wait_for("Settings");
    h.send_keys("/");
    h.type_text("Show shortcut bar");
    h.send_keys("Enter");
    h.send_keys("Enter");
    h.send_keys("C-s");
    let config: toml::Table =
        std::fs::read_to_string(app_dir_in(h.home_path()).join("config.toml"))
            .unwrap()
            .parse()
            .unwrap();
    assert_eq!(
        config["session"]["show_shortcut_bar"].as_bool(),
        Some(false)
    );
    h.send_keys("Escape");
    h.send_keys("Escape");
    h.wait_for_ready();
    h.assert_screen_not_contains("Cmds");
    h.send_keys("?");
    h.wait_for("Keyboard Shortcuts");
    h.send_keys("Escape");
    h.send_keys("C-k");
    h.type_text("Show tips");
    h.send_keys("Enter");
    h.wait_for("Tips");
    h.send_keys("Escape");
    h.kill_tui();
    h.spawn_tui();
    h.wait_for_ready();
    h.assert_screen_not_contains("Cmds");
    h.send_keys("s");
    h.wait_for("Settings");
    h.send_keys("/");
    h.type_text("Show shortcut bar");
    h.send_keys("Enter");
    h.send_keys("Enter");
    h.send_keys("C-s");
    h.send_keys("Escape");
    h.send_keys("Escape");
    h.wait_for("Cmds");
}

#[test]
#[parallel]
fn shortcut_bar_hidden_live_still_relays_input_and_exits() {
    require_tmux!();
    let mut h = TuiTestHarness::new("shortcut_bar_live");
    let config_path = app_dir_in(h.home_path()).join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).unwrap();
    config
        .push_str("\n[session]\nshow_shortcut_bar = false\ndefault_attach_mode = \"live_send\"\n");
    std::fs::write(&config_path, config).unwrap();
    let bin = h.install_path_command("claude");
    std::fs::write(
        bin.join("claude"),
        "#!/bin/sh\nstty -echo -icanon min 1 time 0\nprintf 'agent-ready\\n'\nexec cat\n",
    )
    .unwrap();
    let project = h.project_path();
    h.add_session(&[project.to_str().unwrap(), "-t", "Hidden bar"]);
    h.spawn_tui();
    h.wait_for("Hidden bar");
    h.send_keys("Enter");
    h.wait_for("agent-ready");
    h.assert_screen_not_contains("LIVE");
    h.assert_screen_not_contains("Cmds");
    h.type_text("input-reached-agent");
    h.wait_for("input-reached-agent");
    h.send_keys("C-q");
    h.send_keys("?");
    h.wait_for("Keyboard Shortcuts");
}

#[test]
#[parallel]
fn shortcut_bar_tip_after_cli_creation_waits_for_home_and_is_seen_once() {
    use std::{fs, time::Duration};

    require_tmux!();
    let mut h = TuiTestHarness::new("shortcut_bar_tip");
    let app = app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\nsessions_created = 30\n");
    fs::write(&config_path, config).unwrap();
    h.spawn_tui();
    h.wait_for_ready();
    h.send_keys("?");
    h.wait_for("Keyboard Shortcuts");
    let project = h.project_path();
    let args = [
        "add",
        project.to_str().unwrap(),
        "-t",
        "Threshold session",
        "--cmd-override",
        "sleep 600",
    ];
    h.run_cli_ok(&args);
    let read_state = || -> toml::Table {
        fs::read_to_string(app.join("state.toml"))
            .unwrap()
            .parse()
            .unwrap()
    };
    assert_eq!(read_state()["sessions_created"].as_integer(), Some(31));
    h.run_cli_err(&args);
    assert_eq!(read_state()["sessions_created"].as_integer(), Some(31));
    h.send_keys("Escape");
    h.wait_for_timeout("More room for your sessions", Duration::from_secs(15));
    h.assert_screen_contains("Show shortcut bar");
    h.send_keys("Escape");
    let state = read_state();
    assert!(state["tips_seen"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id.as_str() == Some("hide-shortcut-bar")));
    h.kill_tui();
    h.spawn_tui();
    h.wait_for_ready();
    h.send_keys("?");
    h.wait_for("Keyboard Shortcuts");
    h.send_keys("Escape");
    h.assert_screen_not_contains("More room for your sessions");
    h.assert_screen_contains("Cmds");
}

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
