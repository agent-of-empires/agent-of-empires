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
    let sessions: Vec<agent_of_empires::session::Instance> =
        serde_json::from_slice(&std::fs::read(h.sessions_path()).unwrap()).unwrap();
    let agent_name =
        agent_of_empires::tmux::Session::generate_name(&sessions[0].id, &sessions[0].title);
    // Agent output is also visible in passive preview. Only the live worker
    // claims pane size ownership, so its readiness must precede test input.
    crate::harness::wait_until(
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis(20),
        || {
            let output = h
                .tmux()
                .args([
                    "display-message",
                    "-p",
                    "-t",
                    &agent_name,
                    "#{@aoe_size_owner}",
                ])
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).into_owned());
            }
            let owner = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
            owner
                .trim()
                .starts_with("tui-")
                .then_some(())
                .ok_or_else(|| format!("Native live worker has not claimed {agent_name}: {owner}"))
        },
    );
    h.wait_for("agent-ready");
    h.assert_screen_not_contains("LIVE");
    h.assert_screen_not_contains("Cmds");
    h.type_text("input-reached-agent");
    crate::harness::wait_until(
        std::time::Duration::from_secs(10),
        std::time::Duration::from_millis(20),
        || {
            let output = h
                .tmux()
                .args(["capture-pane", "-p", "-t", &agent_name])
                .output()
                .map_err(|error| error.to_string())?;
            if !output.status.success() {
                return Err(String::from_utf8_lossy(&output.stderr).into_owned());
            }
            let pane = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
            pane.contains("input-reached-agent")
                .then_some(())
                .ok_or_else(|| format!("Live input has not reached the agent pane: {pane}"))
        },
    );
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
    let mut h = TuiTestHarness::new_in_tmp("shortcut_bar_tip");
    h.stop_daemon_on_drop();
    let app = app_dir_in(h.home_path());
    let config_path = app.join("config.toml");
    let mut config = fs::read_to_string(&config_path).unwrap();
    config.push_str("\nsessions_created = 30\n");
    fs::write(&config_path, config).unwrap();
    h.run_cli_ok(&["profile", "create", "other"]);
    h.spawn(&["--profile", "default"]);
    h.wait_for_ready();
    h.send_keys("?");
    h.wait_for("Keyboard Shortcuts");
    let project = h.project_path();
    let args = [
        "--profile",
        "other",
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
    let state = read_state();
    assert!(state["tips_seen"]
        .as_array()
        .unwrap()
        .iter()
        .any(|id| id.as_str() == Some("hide-shortcut-bar")));
    h.kill_tui();
    h.spawn(&["--profile", "default"]);
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
