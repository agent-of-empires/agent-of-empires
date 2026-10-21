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

    let flag = h.run_cli_err(&["--no-autostart", "acp", "status", "no-such-session"]);
    assert!(
        flag.contains("no structured view daemon is running"),
        "{flag}"
    );

    h.set_env("AOE_NO_AUTOSTART", "1");
    let env = h.run_cli_err(&["acp", "status", "no-such-session"]);
    assert!(
        env.contains("no structured view daemon is running"),
        "{env}"
    );

    assert!(
        !app_dir_in(h.home_path()).join("serve.pid").exists(),
        "an opted-out client must not start a daemon"
    );
}
