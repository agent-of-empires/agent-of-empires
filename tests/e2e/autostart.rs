//! A CLI command that needs the daemon starts one, exactly once under a race,
//! and `--no-autostart` / `AOE_NO_AUTOSTART` opt out.

use serial_test::parallel;

use crate::harness::{app_dir_in, TuiTestHarness};

const STARTED: &str = "Started aoe daemon (pid ";

#[test]
#[parallel]
fn racing_clients_start_exactly_one_daemon() {
    let mut h = TuiTestHarness::new_in_tmp("autostart_race");
    h.stop_daemon_on_drop();

    let children: Vec<_> = (0..4)
        .map(|_| h.spawn_cli(&["acp", "status", "no-such-session"]))
        .collect();
    let started: Vec<String> = children
        .into_iter()
        .map(|child| {
            let out = child.wait_with_output().expect("wait for aoe");
            String::from_utf8_lossy(&out.stderr).into_owned()
        })
        .filter(|stderr| stderr.contains(STARTED))
        .collect();
    assert_eq!(
        started.len(),
        1,
        "one winner announces the start: {started:?}"
    );

    let pid_path = app_dir_in(h.home_path()).join("serve.pid");
    let pid = std::fs::read_to_string(pid_path).expect("serve.pid after autostart");
    assert!(
        started[0].contains(&format!("{STARTED}{})", pid.trim())),
        "announced pid matches serve.pid {pid}: {}",
        started[0]
    );
}

#[test]
#[parallel]
fn opt_out_hard_fails_without_spawning() {
    let mut h = TuiTestHarness::new_in_tmp("autostart_opt_out");
    h.stop_daemon_on_drop();

    h.run_cli_err(&["--no-autostart", "acp", "status", "no-such-session"]);
    h.set_env("AOE_NO_AUTOSTART", "1");
    h.run_cli_err(&["acp", "status", "no-such-session"]);

    assert!(
        !app_dir_in(h.home_path()).join("serve.pid").exists(),
        "an opted-out client must not start a daemon"
    );
}

#[test]
#[parallel]
fn native_opt_out_refuses_spawn_and_reuses_existing_core() {
    crate::harness::require_tmux!();
    for use_env in [false, true] {
        let mut h = TuiTestHarness::new_in_tmp(if use_env {
            "native_autostart_env"
        } else {
            "native_autostart_flag"
        });
        h.stop_daemon_on_drop();
        h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
        if use_env {
            h.set_env("AOE_NO_AUTOSTART", "1");
            h.spawn_tui();
        } else {
            h.spawn(&["--no-autostart"]);
        }
        h.wait_for("Not responding");
        let app = app_dir_in(h.home_path());
        assert!(!app.join("serve.pid").exists(), "opt-out started a daemon");
        assert!(
            !app.join("daemon/api.sock").exists(),
            "opt-out created a native listener"
        );

        h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
        let original_pid = std::fs::read_to_string(app.join("serve.pid")).unwrap();
        h.send_keys("r");
        h.wait_for_runtime_ready();
        assert_eq!(
            std::fs::read_to_string(app.join("serve.pid")).unwrap(),
            original_pid,
            "reconnect replaced the existing owner"
        );
        let launch: serde_json::Value =
            serde_json::from_slice(&std::fs::read(app.join("serve.launch")).unwrap()).unwrap();
        assert_eq!(launch["core_only"], true, "reconnect changed exposure");
    }
}
