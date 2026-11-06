//! Config commands write through the Daemon: they auto-start one, and with
//! `--no-autostart` they fail without touching any file.

use serial_test::parallel;

use crate::harness::{app_dir_in, init_git_repo, TuiTestHarness};

#[test]
#[parallel]
fn config_commands_write_through_an_autostarted_daemon() {
    let mut h = TuiTestHarness::new_in_tmp("daemon_config_flow");
    h.stop_daemon_on_drop();
    h.remove_env("DO_NOT_TRACK");

    let profiles = app_dir_in(h.home_path()).join("profiles");
    h.run_cli_ok(&["profile", "create", "alpha"]);
    assert!(profiles.join("alpha").exists());
    h.run_cli_ok(&["profile", "rename", "alpha", "beta"]);
    assert!(profiles.join("beta").exists() && !profiles.join("alpha").exists());
    h.run_cli_ok(&["profile", "default", "beta"]);
    assert!(h
        .run_cli_ok(&["profile", "default"])
        .contains("Default profile: beta"));
    let missing = h.run_cli_err(&["profile", "default", "ghost"]);
    assert!(missing.contains("does not exist"), "{missing}");

    // A relative path is anchored in the CLI's directory, not the Daemon's.
    let repo = h.home_path().join("repo");
    init_git_repo(&repo);
    let added = h
        .cli_command(&["-p", "beta", "project", "add", "repo"])
        .current_dir(h.home_path())
        .output()
        .expect("run aoe project add");
    assert!(added.status.success(), "{added:?}");
    let listed = h.run_cli_ok(&["-p", "beta", "project", "list", "--scope", "profile"]);
    assert!(
        listed.contains("repo") && listed.contains("[profile]"),
        "{listed}"
    );
    h.run_cli_ok(&["-p", "beta", "project", "remove", "repo"]);
    assert!(h
        .run_cli_ok(&["-p", "beta", "project", "list", "--scope", "profile"])
        .contains("No projects registered"));

    h.run_cli_ok(&["profile", "create", "scoped"]);
    let repo = repo.to_str().unwrap();
    h.run_cli_ok(&["-p", "scoped", "project", "add", repo]);
    let conflict = h.run_cli_err(&["-p", "scoped", "project", "add", repo, "--scope", "global"]);
    assert!(conflict.contains("allow-override"), "{conflict}");
    h.run_cli_ok(&[
        "-p",
        "scoped",
        "project",
        "add",
        repo,
        "--scope",
        "global",
        "--allow-override",
    ]);
    let global = h.run_cli_ok(&["project", "list", "--scope", "global"]);
    assert!(global.contains("repo"), "{global}");

    let enabled = h.run_cli_ok(&["telemetry", "enable"]);
    let first = enabled.lines().find_map(|l| l.split("install id: ").nth(1));
    let first = first.expect("install id printed on enable").to_string();
    let reset = h.run_cli_ok(&["telemetry", "reset-id"]);
    assert!(
        reset.contains("fresh anonymous install id") && !reset.contains(&first),
        "{reset}"
    );
    assert!(h
        .run_cli_ok(&["telemetry", "disable"])
        .contains("install id has been deleted"));

    let bundle = "schema_version = 1\n[settings.session]\ntrash_retention_minutes = 0\n";
    let applied = h.run_cli_with_stdin(&["cityhall", "apply", "-"], bundle);
    assert!(applied.status.success(), "{applied:?}");
    assert!(String::from_utf8_lossy(&applied.stdout).contains("Applied 1 settings."));
    let rejected = h.run_cli_with_stdin(&["cityhall", "apply", "-"], "schema_version = 99\n");
    assert!(!rejected.status.success());
}

#[test]
#[parallel]
fn config_commands_fail_without_a_daemon_when_autostart_is_off() {
    let mut h = TuiTestHarness::new_in_tmp("daemon_config_opt_out");
    h.stop_daemon_on_drop();

    let cases: [&[&str]; 4] = [
        &["profile", "create", "alpha"],
        &["telemetry", "enable"],
        &["skill", "add", "review"],
        &["plugin", "disable", "aoe.web"],
    ];
    for args in cases {
        for flag in [false, true] {
            let mut command = h.cli_command(args);
            if flag {
                command.arg("--no-autostart");
            } else {
                command.env("AOE_NO_AUTOSTART", "1");
            }
            let output = command.output().unwrap();
            assert!(!output.status.success(), "{args:?}: {output:?}");
        }
    }
    let app = app_dir_in(h.home_path());
    assert!(!app.join("profiles").join("alpha").exists());
    assert!(!app.join("serve.pid").exists());
}

#[test]
#[parallel]
fn config_writes_repair_missing_defaults_without_minting_profiles() {
    for (empty, warm) in [(false, false), (false, true), (true, false), (true, true)] {
        let mut h = TuiTestHarness::new_in_tmp("daemon_config_repair");
        h.stop_daemon_on_drop();
        h.run_cli_ok(&["migrate"]);
        let app = app_dir_in(h.home_path());
        let config_path = app.join("config.toml");
        let mut config: toml::Value =
            toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
        config["default_profile"] = toml::Value::String("missing".into());
        std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
        let profiles = app.join("profiles");
        if empty {
            std::fs::remove_dir_all(&profiles).unwrap();
            std::fs::create_dir(&profiles).unwrap();
        }
        let existing_pid = if warm {
            h.run_cli_ok(&["profile", "list"]);
            Some(std::fs::read_to_string(app.join("serve.pid")).unwrap())
        } else {
            None
        };
        let project = h.project_path();
        h.run_cli_ok(&[
            "project",
            "add",
            project.to_str().unwrap(),
            "--scope",
            "global",
            "--name",
            "repair",
        ]);
        assert!(!profiles.join("missing").exists());
        let listed = h.run_cli_ok(&["project", "list", "--scope", "global"]);
        assert!(listed.contains("repair"), "{listed}");
        h.run_cli_ok(&["profile", "create", "repaired"]);
        h.run_cli_ok(&["profile", "default", "repaired"]);
        let saved: toml::Value =
            toml::from_str(&std::fs::read_to_string(config_path).unwrap()).unwrap();
        assert_eq!(saved["default_profile"].as_str(), Some("repaired"));
        assert!(profiles.join("repaired").is_dir());
        assert!(!profiles.join("missing").exists());
        if empty {
            assert_eq!(std::fs::read_dir(&profiles).unwrap().count(), 1);
        }
        if let Some(pid) = existing_pid {
            assert_eq!(std::fs::read_to_string(app.join("serve.pid")).unwrap(), pid);
        }
    }
}
