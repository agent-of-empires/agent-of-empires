//! Full-stack e2e for #3576: two Pi sessions in ONE project directory each
//! launch with their own pinned conversation id, and a restart re-pins the
//! same id with the flag that creates it.
//!
//! Teeth: the shim records every launch's argv and writes no session file,
//! the shape of a pane nobody prompted. Without the fix a fresh launch carries
//! no id and a restart emits `--session`, which pi exits 1 on for an id it has
//! never recorded. Launches correlate by pinned id, not `AOE_INSTANCE_ID`,
//! which only reaches panes whose agent declares hooks; Pi declares none.
//!
//! Daemon-free, so no feature gate. Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- pi_pinned_session_id --nocapture
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// Install a `pi` stub that answers the capability probe, appends every
/// launch's argv to `$HOME/pi-launches`, and then parks so the pane stays
/// alive. It deliberately creates no session file: a real pi writes one on the
/// first message, and a pane nobody prompted is the case whose identity has to
/// survive a restart.
fn install_pi_shim(h: &mut TuiTestHarness) {
    let bin = h.install_path_command("pi");
    let script = r#"#!/bin/sh
case "$1" in
  --help)
    # Mirrors the flag list of pi 0.76.0+, which is what the launch probes for.
    echo "Options:"
    echo "  --session <path|id>            Use specific session file or partial UUID"
    echo "  --session-id <id>              Use exact project session ID, creating it if missing"
    exit 0
    ;;
esac
printf '%s ' "$@" >> "$HOME/pi-launches"
printf '\n' >> "$HOME/pi-launches"
exec sleep 600
"#;
    let path = bin.join("pi");
    std::fs::write(&path, script).expect("write pi shim");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

/// Every profile's `sessions.json`, so the lookup does not depend on which
/// profile name the CLI happened to create.
fn session_stores(h: &TuiTestHarness) -> Vec<PathBuf> {
    let profiles = app_dir_in(h.home_path()).join("profiles");
    std::fs::read_dir(&profiles)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("sessions.json"))
        .filter(|path| path.exists())
        .collect()
}

fn agent_session_id_of(h: &TuiTestHarness, instance_id: &str) -> Option<String> {
    for store in session_stores(h) {
        let content = std::fs::read_to_string(&store).unwrap_or_default();
        let sessions: Value = serde_json::from_str(&content).unwrap_or(Value::Null);
        let found = sessions.as_array().and_then(|rows| {
            rows.iter()
                .find(|r| r["id"].as_str() == Some(instance_id))?
                .get("agent_session_id")?
                .as_str()
                .map(str::to_owned)
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

/// Deadline and cadence for waiting on the shim, which runs asynchronously
/// inside the tmux pane after the CLI launch returns.
const SHIM_DEADLINE: Duration = Duration::from_secs(20);
const SHIM_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Block until the shim has recorded `expected` launches, then return them
/// oldest first.
fn wait_for_launches(h: &TuiTestHarness, expected: usize) -> Vec<String> {
    let path = h.home_path().join("pi-launches");
    let deadline = Instant::now() + SHIM_DEADLINE;
    loop {
        let lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap_or_default()
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_owned)
            .collect();
        if lines.len() >= expected {
            return lines;
        }
        if Instant::now() >= deadline {
            panic!(
                "shim recorded {} of {expected} launches within {SHIM_DEADLINE:?}\n\
                 recorded: {lines:#?}",
                lines.len()
            );
        }
        std::thread::sleep(SHIM_POLL_INTERVAL);
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

/// Parse the `  ID:      <id>` line that `aoe add` prints on success.
fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("`aoe add` printed no ID line"))
}

fn add_session(h: &TuiTestHarness, project: &std::path::Path, title: &str) -> String {
    let add = h.run_cli(&[
        "add",
        project.to_str().expect("utf8 project"),
        "-t",
        title,
        "-c",
        "pi",
    ]);
    assert!(
        add.status.success(),
        "aoe add {title} failed.\nstderr: {}",
        String::from_utf8_lossy(&add.stderr),
    );
    parse_session_id(&String::from_utf8_lossy(&add.stdout))
}

fn run_session_cli(h: &TuiTestHarness, command: &str, instance_id: &str) {
    let out = h.run_cli(&["session", command, instance_id]);
    assert!(
        out.status.success(),
        "aoe session {command} failed.\nstderr: {}",
        String::from_utf8_lossy(&out.stderr),
    );
}

#[test]
#[parallel]
fn pi_pinned_session_id() {
    require_tmux!();

    let mut h = TuiTestHarness::new_in_tmp("pi_pinned_session_id");
    let pi_home = h.home_path().join(".pi/agent");
    std::fs::create_dir_all(&pi_home).expect("mkdir pi home");
    h.set_env("PI_CODING_AGENT_DIR", &pi_home.display().to_string());
    install_pi_shim(&mut h);

    // ONE project directory, two sessions: the #3576 shape.
    let project = h.home_path().join("shared-project");
    std::fs::create_dir_all(&project).expect("mkdir project");

    let id_a = add_session(&h, &project, "pi-A");
    let id_b = add_session(&h, &project, "pi-B");

    // Started one at a time so each recorded launch is attributable to the
    // session that produced it: a set comparison would also pass if the two
    // panes had swapped conversations, which is the failure being guarded.
    run_session_cli(&h, "start", &id_a);
    let launch_a = wait_for_launches(&h, 1).remove(0);
    run_session_cli(&h, "start", &id_b);
    let launch_b = wait_for_launches(&h, 2).remove(1);

    // Each pane was launched with its own pinned conversation, and those are
    // the ids AoE persisted. Sharing a directory changes nothing, because
    // nothing consulted the directory.
    let pinned_a = flag_value(&launch_a, "--session-id")
        .unwrap_or_else(|| panic!("session A launch carried no pinned --session-id: {launch_a}"));
    let pinned_b = flag_value(&launch_b, "--session-id")
        .unwrap_or_else(|| panic!("session B launch carried no pinned --session-id: {launch_b}"));
    assert_ne!(
        pinned_a, pinned_b,
        "co-located panes must pin different conversations"
    );

    let stored_a = agent_session_id_of(&h, &id_a).expect("session A persisted no id");
    let stored_b = agent_session_id_of(&h, &id_b).expect("session B persisted no id");
    assert_eq!(
        stored_a, pinned_a,
        "session A must persist the id its own pane launched with"
    );
    assert_eq!(
        stored_b, pinned_b,
        "session B must persist the id its own pane launched with"
    );

    // Restart: the store still holds nothing for either id, because neither
    // pane was ever prompted. The relaunch must therefore re-pin the same
    // conversation with the flag that creates it; `--session` exits 1 on an id
    // pi has never recorded, and the id must not drift onto the peer's.
    run_session_cli(&h, "stop", &id_a);
    run_session_cli(&h, "start", &id_a);

    let relaunch = wait_for_launches(&h, 3).remove(2);
    assert_eq!(
        flag_value(&relaunch, "--session-id").as_deref(),
        Some(&*stored_a),
        "a restart must re-pin the same conversation with the creating flag, got: {relaunch}"
    );
    assert_eq!(agent_session_id_of(&h, &id_a).as_deref(), Some(&*stored_a));
}
