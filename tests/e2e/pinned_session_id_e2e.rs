//! #3576 and #3638: two sessions of the same agent in ONE project directory
//! each launch with their own pinned conversation id, and a restart re-pins the
//! same id with the flag that creates it.
//!
//! Each shim records every launch's argv and writes no session file or
//! transcript, which is the shape of a pane nobody prompted: nothing on disk
//! can be scanned for an identity, so the only source is the flag AoE put on
//! the launch line. Launches correlate by pinned id, not `AOE_INSTANCE_ID`,
//! which only reaches panes whose agent declares hooks.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, write_executable, TuiTestHarness};

const SHIM_DEADLINE: Duration = Duration::from_secs(20);

/// Look in every profile's `sessions.json`, so the lookup does not depend on
/// which profile name the CLI happened to create.
fn agent_session_id_of(h: &TuiTestHarness, instance_id: &str) -> Option<String> {
    let profiles = app_dir_in(h.home_path()).join("profiles");
    let stores: Vec<PathBuf> = std::fs::read_dir(&profiles)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("sessions.json"))
        .filter(|path| path.exists())
        .collect();
    stores.iter().find_map(|store| {
        let content = std::fs::read_to_string(store).unwrap_or_default();
        let sessions: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
        crate::harness::agent_session_id_of(&sessions, instance_id)
    })
}

/// Block until the shim has recorded `expected` launches, then return the last.
fn wait_for_launch(h: &TuiTestHarness, launches: &str, expected: usize) -> String {
    let path = h.home_path().join(launches);
    let deadline = Instant::now() + SHIM_DEADLINE;
    loop {
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect();
        if lines.len() >= expected {
            return lines[expected - 1].clone();
        }
        assert!(
            Instant::now() < deadline,
            "shim recorded {} of {expected} launches within {SHIM_DEADLINE:?}\nrecorded: {lines:#?}",
            lines.len()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The value passed after `flag` in a recorded launch line.
fn flag_value(argv_line: &str, flag: &str) -> Option<String> {
    let mut words = argv_line.split_whitespace();
    while let Some(word) = words.next() {
        if word == flag {
            return words.next().map(str::to_owned);
        }
    }
    None
}

/// Start two co-located sessions one at a time (so each launch is attributable
/// to the session that produced it), then restart the first.
fn assert_pins_survive_restart(h: &TuiTestHarness, add_args: &[&str], launches: &str) {
    let project = h.home_path().join("shared-project");
    std::fs::create_dir_all(&project).expect("mkdir project");
    let project = project.to_str().unwrap();
    let add = |title: &str| {
        let mut args = vec![project, "-t", title];
        args.extend_from_slice(add_args);
        h.add_session(&args)
    };
    let id_a = add("shared-A");
    let id_b = add("shared-B");

    h.run_cli_ok(&["session", "start", &id_a]);
    let launch_a = wait_for_launch(h, launches, 1);
    h.run_cli_ok(&["session", "start", &id_b]);
    let launch_b = wait_for_launch(h, launches, 2);

    let pinned = |line: &str| {
        flag_value(line, "--session-id")
            .unwrap_or_else(|| panic!("launch carried no pinned --session-id: {line}"))
    };
    assert_ne!(
        pinned(&launch_a),
        pinned(&launch_b),
        "co-located panes must pin different conversations"
    );
    let stored_a = agent_session_id_of(h, &id_a).expect("session A persisted no id");
    let stored_b = agent_session_id_of(h, &id_b).expect("session B persisted no id");
    assert_eq!(stored_a, pinned(&launch_a));
    assert_eq!(stored_b, pinned(&launch_b));

    // Neither pane was prompted, so nothing was recorded to resume from: the
    // relaunch must re-pin the same conversation with the creating flag
    // (`--session` exits 1 on an id the agent never recorded).
    h.run_cli_ok(&["session", "stop", &id_a]);
    h.run_cli_ok(&["session", "start", &id_a]);
    let relaunch = wait_for_launch(h, launches, 3);
    assert_eq!(
        flag_value(&relaunch, "--session-id").as_deref(),
        Some(&*stored_a),
        "a restart must re-pin the same conversation, got: {relaunch}"
    );
    assert_eq!(agent_session_id_of(h, &id_a).as_deref(), Some(&*stored_a));
    assert_eq!(
        agent_session_id_of(h, &id_b).as_deref(),
        Some(&*stored_b),
        "restarting a peer must not move the other session's conversation"
    );
}

/// A custom agent mapped onto a supported one (`custom_agents` +
/// `agent_detect_as`) resolves through the alias rather than the raw
/// `Instance::tool`, which named no built-in and so pinned nothing (#3638).
#[test]
#[parallel]
fn custom_agent_resume() {
    require_tmux!();
    const CUSTOM_AGENT: &str = "claude-personal";
    let mut h = TuiTestHarness::new_in_tmp("custom_agent_resume");
    // The `launch:` prefix keeps an argv-less launch on its own recorded line.
    let bin = h.install_path_command("claude");
    write_executable(
        &bin.join("claude"),
        "#!/bin/sh\nprintf 'launch: %s\\n' \"$*\" >> \"$HOME/alias-launches\"\nexec sleep 600\n",
    );
    // A differently named or path-qualified wrapper fails closed, so the alias
    // maps onto the plain `claude` command the launch PATH resolves.
    h.append_config(&format!(
        "[session]\n\
         custom_agents = {{ \"{CUSTOM_AGENT}\" = \"claude\" }}\n\
         agent_detect_as = {{ \"{CUSTOM_AGENT}\" = \"claude\" }}"
    ));

    // `--tool` keeps the custom-agent identity; `--cmd claude` would select the
    // built-in and skip alias resolution.
    assert_pins_survive_restart(&h, &["--tool", CUSTOM_AGENT], "alias-launches");
}

/// Pi declares no hooks, so its panes carry no `AOE_INSTANCE_ID` (#3576).
#[test]
#[parallel]
fn pi_pinned_session_id() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("pi_pinned_session_id");
    let pi_home = h.home_path().join(".pi/agent");
    std::fs::create_dir_all(&pi_home).expect("mkdir pi home");
    h.set_env("PI_CODING_AGENT_DIR", pi_home.to_str().unwrap());
    let bin = h.install_path_command("pi");
    // The `--help` branch mirrors pi 0.76.0+, which is what the launch probes.
    write_executable(
        &bin.join("pi"),
        r#"#!/bin/sh
case "$1" in
  --help)
    echo "Options:"
    echo "  --session <path|id>            Use specific session file or partial UUID"
    echo "  --session-id <id>              Use exact project session ID, creating it if missing"
    exit 0
    ;;
esac
printf '%s ' "$@" >> "$HOME/pi-launches"
printf '\n' >> "$HOME/pi-launches"
exec sleep 600
"#,
    );

    assert_pins_survive_restart(&h, &["-c", "pi"], "pi-launches");
}
