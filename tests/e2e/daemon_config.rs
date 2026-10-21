//! Config commands write through the Daemon: they auto-start one, and with
//! `--no-autostart` they fail without touching any file.

use serial_test::parallel;

use crate::harness::{app_dir_in, init_git_repo, TuiTestHarness};

#[test]
#[parallel]
fn config_commands_write_through_an_autostarted_daemon() {
    let mut h = TuiTestHarness::new_in_tmp("daemon_config_flow");
    h.stop_daemon_on_drop();

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
        let stderr = h.run_cli_err(&[&["--no-autostart"], args].concat());
        assert!(
            stderr.contains("no local daemon is running"),
            "{args:?}: {stderr}"
        );
    }
    let app = app_dir_in(h.home_path());
    assert!(!app.join("profiles").join("alpha").exists());
    assert!(!app.join("serve.pid").exists());
}
