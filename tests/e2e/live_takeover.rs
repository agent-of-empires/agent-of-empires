//! E2E for the live-send takeover exit: when another surface steals the
//! size-owner lock (what the web dashboard's "take over" does), the TUI
//! must exit live mode on its own, without a keystroke, and explain who
//! took over. Guards against the old silent tug-of-war where the TUI
//! re-stole the lock on the next input batch and reverted the web's grid.

use serial_test::parallel;
use std::process::Command;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// Replace the pre-seeded config with one that routes activation into
/// live-send (mirrors `new_session::write_config_attach_mode_live_send`).
fn write_live_send_config(h: &TuiTestHarness) {
    let config_dir = app_dir_in(h.home_path());
    let config_content = format!(
        r#"[updates]
update_check_mode = "off"

[app_state]
has_seen_welcome = true
has_responded_to_telemetry = true
last_seen_version = "{version}"
has_acknowledged_agent_hooks = true

[session]
default_attach_mode = "live_send"
"#,
        version = env!("CARGO_PKG_VERSION"),
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write live-send config");
}

fn list_sessions_on(socket: &std::path::Path) -> Vec<String> {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

fn set_session_option(socket: &std::path::Path, session: &str, opt: &str, value: &str) {
    let out = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["set-option", "-t", session, opt, value])
        .output()
        .expect("tmux set-option");
    assert!(
        out.status.success(),
        "tmux set-option {opt} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[parallel]
fn test_web_takeover_exits_tui_live_mode_with_dialog() {
    require_tmux!();

    let mut h = TuiTestHarness::new("live_takeover");
    write_live_send_config(&h);

    // The default `claude` stub exits immediately, which kills the agent
    // pane before live mode can observe anything (the known short-lived-
    // shell flake). Shadow it with a long-running stand-in so the pane
    // survives the whole test.
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nsleep 300\n").expect("write sleeping claude");

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Takeover"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("Takeover");

    // Activate the session; default_attach_mode routes into live-send.
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(10));

    // Find the agent's tmux session on the harness socket (the harness's
    // own UI session is `aoe_e2e_*`; agent sessions carry the dev prefix).
    let socket = h.home_path().join("tmux.sock");
    let agent_session = list_sessions_on(&socket)
        .into_iter()
        .find(|name| name.starts_with("aoe_dev_") && !name.starts_with("aoe_e2e_"))
        .expect("agent tmux session exists");

    // Steal the size-owner lock the way the web dashboard's Claim handler
    // does (`steal_size_owner`): overwrite owner + fresh heartbeat with a
    // `live-*` id.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        .to_string();
    set_session_option(&socket, &agent_session, "@aoe_size_owner", "live-e2e-web");
    set_session_option(&socket, &agent_session, "@aoe_size_owner_hb", &now_ms);

    // With NO further input, the worker's idle heartbeat notices the loss
    // (<= 1.5s), the main-loop poll exits live mode, and the dialog names
    // the web dashboard. The LIVE badge must be gone: live mode ended.
    h.wait_for_timeout("Live send ended", Duration::from_secs(10));
    h.assert_screen_contains("web dashboard");
    h.wait_for_absent("LIVE", Duration::from_secs(5));

    // The thief keeps the lock: the TUI must not have stolen it back.
    let owner = Command::new("tmux")
        .arg("-S")
        .arg(&socket)
        .args([
            "show-options",
            "-v",
            "-t",
            &agent_session,
            "@aoe_size_owner",
        ])
        .output()
        .expect("tmux show-options");
    assert_eq!(
        String::from_utf8_lossy(&owner.stdout).trim(),
        "live-e2e-web",
        "TUI must not re-steal the size-owner lock after takeover"
    );
}
