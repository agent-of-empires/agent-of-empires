//! e2e coverage for live send attaching a pane whose status is unclassified.
//!
//! An agent pane can be alive while its screen is unreadable to the status
//! detector: a custom command runs through a wrapper script, so the pane never
//! matches an agent manifest, and until the program paints enough content the
//! legacy detector lands on `Status::Unknown`. That is a live pane, but the
//! receipt gate used to accept only `Running | Waiting | Idle | Starting`, so
//! the TUI refused the attachment it had just prepared with
//! "Prepared target no longer matches the applied snapshot".
//!
//! Unit tests pin the allowlist; only an e2e proves the whole keystroke path --
//! real binary, real tmux, a real daemon polling a real pane -- reaches the
//! agent. The pane must be pre-launched so its status is genuinely `Unknown` at
//! Enter: a cold start reports `Starting`, which was already allowed, and would
//! make this test pass on the broken build.

use serial_test::parallel;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, wait_until, TuiTestHarness};

/// Route activation into live-send and make `quiet` the only launchable agent,
/// so the created session takes the unclassified-status path.
fn write_unclassified_config(h: &TuiTestHarness, agent: &std::path::Path) {
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
default_tool = "quiet"
custom_agents = {{ quiet = {agent:?} }}

{systemd_off}"#,
        version = env!("CARGO_PKG_VERSION"),
        agent = agent.to_str().expect("agent stub path"),
        systemd_off = crate::harness::SYSTEMD_OFF,
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write config");
}

/// An unclassified pane must still take live input, and must take it exactly
/// once: a retry that duplicated the keystrokes would show up as a doubled
/// marker in the pane, which is the failure mode that makes a "works on my
/// machine" fix dangerous.
#[test]
#[parallel]
fn live_send_attaches_a_pane_whose_status_is_unclassified() {
    require_tmux!();

    // `new_in_tmp` keeps the home under `/tmp`: this test lets the CLI autostart
    // a daemon, and its unix socket path overflows Darwin's 104-byte `sun_path`
    // when the home sits under `/var/folders/.../TmpDir`.
    let mut h = TuiTestHarness::new_in_tmp("live_send_unknown_status");

    // Stand in for an agent whose TUI the detector cannot read yet: it echoes
    // what it receives and never enables bracketed paste, so the marker text
    // lands verbatim and can be counted. The file name shares no word with the
    // tool name, so echoing the marker cannot make the pane look like the agent
    // and flip the status away from `Unknown`.
    let agent = h.home_path().join("agent-stub.sh");
    crate::harness::write_executable(
        &agent,
        "#!/bin/sh\nstty -echo -icanon min 1 time 0 2>/dev/null\nexec cat -v\n",
    );
    write_unclassified_config(&h, &agent);

    let project = h.project_path();
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "Unclassified",
        "--tool",
        "quiet",
        "--launch",
    ]);
    assert!(
        add.status.success(),
        "aoe add failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );

    // The premise of the test: the pane is up and its status is unclassified.
    // A detour into `idle` (pane content the detector can read) or `stopped`
    // (the stub exited) would exercise a different branch, so fail loudly here
    // instead of asserting something weaker further down.
    wait_until(Duration::from_secs(30), Duration::from_millis(100), || {
        let sessions = h.try_read_sessions();
        let status = sessions
            .as_array()
            .and_then(|rows| {
                rows.iter()
                    .find(|row| row["title"].as_str() == Some("Unclassified"))
            })
            .and_then(|row| row["status"].as_str())
            .map(str::to_owned);
        match status.as_deref() {
            Some("unknown") => Ok(()),
            other => Err(format!("status is {other:?}, not an unclassified pane")),
        }
    });

    h.spawn_tui();
    h.wait_for_runtime_ready();
    h.wait_for("Unclassified");
    h.send_keys("Enter");

    // Entry only completes once the receipt's snapshot satisfies the gate; a
    // refusal paints "Live send failed" over the home view instead.
    h.wait_for_timeout("LIVE", Duration::from_secs(10));
    h.assert_screen_not_contains("Live send failed");
    h.assert_screen_not_contains("no longer matches");

    let socket = h.home_path().join("tmux.sock");
    let session = crate::live_send_paste_e2e::agent_session_on(&socket);

    h.type_text("LIVE_SEND_OK");
    h.send_keys("Enter");

    let pane = wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
        let pane = crate::live_send_paste_e2e::capture(&socket, &session);
        if pane.contains("LIVE_SEND_OK") {
            Ok(pane)
        } else {
            Err(format!("marker never reached the agent pane:\n{pane}"))
        }
    });
    assert_eq!(
        pane.matches("LIVE_SEND_OK").count(),
        1,
        "live input must reach an unclassified pane exactly once:\n{pane}"
    );
}
