//! Full-stack e2e for #3638: a custom agent mapped to a supported one
//! (`custom_agents` + `agent_detect_as`) launches with its own pinned
//! conversation id and preserves that identity across restart.
//!
//! Teeth: the shim records every launch's argv and writes no transcript, so
//! nothing on disk can be scanned for an identity. Capture and resume used to
//! key off `Instance::tool` raw, which names the custom alias and resolves to
//! no built-in, so the alias launched with no id, stored none, and started a
//! fresh conversation on every restart.
//!
//! Daemon-free, so no feature gate. Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- custom_agent_resume --nocapture
//! ```

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

const CUSTOM_AGENT: &str = "claude-personal";
const SHIM_BINARY: &str = "claude";

/// Install a Claude-shaped stub that appends every launch's argv to
/// `$HOME/alias-launches` and then parks so the pane stays alive. It writes
/// no Claude transcript: a pane nobody prompted is the case whose identity has
/// to survive a restart, and the only place it can come from is the flag AoE
/// put on the launch line.
fn install_alias_shim(h: &mut TuiTestHarness) {
    let bin = h.install_path_command(SHIM_BINARY);
    // The `launch:` prefix keeps an argv-less launch on its own recorded line,
    // so a launch that pinned nothing fails on the missing flag rather than
    // reading as no launch at all.
    let script = r#"#!/bin/sh
printf 'launch: %s\n' "$*" >> "$HOME/alias-launches"
exec sleep 600
"#;
    let path = bin.join(SHIM_BINARY);
    std::fs::write(&path, script).expect("write alias shim");
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod shim");
}

/// Declare a custom agent whose direct Claude command inherits Claude's
/// behavior. Differently named or path-qualified wrappers fail closed; the
/// test shim stands in for the `claude` command resolved by the launch `PATH`.
fn declare_custom_agent(h: &TuiTestHarness) {
    let app_dir = app_dir_in(h.home_path());
    std::fs::create_dir_all(&app_dir).expect("create app dir");
    let config_path = app_dir.join("config.toml");
    let mut config = std::fs::read_to_string(&config_path).expect("read harness config.toml");
    config.push_str(&format!(
        "\n[session]\n\
         custom_agents = {{ \"{CUSTOM_AGENT}\" = \"{SHIM_BINARY}\" }}\n\
         agent_detect_as = {{ \"{CUSTOM_AGENT}\" = \"claude\" }}\n"
    ));
    std::fs::write(config_path, config).expect("extend config.toml");
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
    let path = h.home_path().join("alias-launches");
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

fn add_session(h: &TuiTestHarness, project: &Path, title: &str) -> String {
    // `--tool` keeps the configured custom-agent identity. `--cmd claude`
    // would select the built-in directly and would not exercise alias resolution.
    let add = h.run_cli(&[
        "add",
        project.to_str().expect("utf8 project"),
        "-t",
        title,
        "--tool",
        CUSTOM_AGENT,
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
fn custom_agent_resume() {
    require_tmux!();

    let mut h = TuiTestHarness::new_in_tmp("custom_agent_resume");
    install_alias_shim(&mut h);
    declare_custom_agent(&h);

    // Two alias sessions in ONE project directory, so a launch that picked
    // up the wrong conversation would show as a shared id.
    let project = h.home_path().join("shared-project");
    std::fs::create_dir_all(&project).expect("mkdir project");

    let id_a = add_session(&h, &project, "alias-A");
    let id_b = add_session(&h, &project, "alias-B");

    // Started one at a time so each recorded launch is attributable to the
    // session that produced it.
    run_session_cli(&h, "start", &id_a);
    let launch_a = wait_for_launches(&h, 1).remove(0);
    run_session_cli(&h, "start", &id_b);
    let launch_b = wait_for_launches(&h, 2).remove(1);

    let pinned_a = flag_value(&launch_a, "--session-id")
        .unwrap_or_else(|| panic!("alias A launched with no conversation id: {launch_a}"));
    let pinned_b = flag_value(&launch_b, "--session-id")
        .unwrap_or_else(|| panic!("alias B launched with no conversation id: {launch_b}"));
    assert_ne!(
        pinned_a, pinned_b,
        "co-located aliases must pin different conversations"
    );

    let stored_a = agent_session_id_of(&h, &id_a).expect("alias A persisted no id");
    let stored_b = agent_session_id_of(&h, &id_b).expect("alias B persisted no id");
    assert_eq!(stored_a, pinned_a);
    assert_eq!(stored_b, pinned_b);

    // The shim writes no transcript. Relaunching the same pin avoids a
    // guaranteed-failing `--resume` while preserving the conversation identity.
    run_session_cli(&h, "stop", &id_a);
    run_session_cli(&h, "start", &id_a);
    let relaunch = wait_for_launches(&h, 3).remove(2);
    assert_eq!(
        flag_value(&relaunch, "--session-id").as_deref(),
        Some(&*stored_a),
        "an empty-thread restart must preserve the session's pinned identity, got: {relaunch}"
    );
    assert_eq!(agent_session_id_of(&h, &id_a).as_deref(), Some(&*stored_a));
    assert_eq!(
        agent_session_id_of(&h, &id_b).as_deref(),
        Some(&*stored_b),
        "restarting a peer must not move the other session's conversation"
    );
}
