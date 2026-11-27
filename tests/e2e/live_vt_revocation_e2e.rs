//! A pending preview arm must not restore native input after runtime authority is revoked.

use std::time::Duration;

use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, wait_until, TuiTestHarness};

fn tmux_supports_pipe_input() -> bool {
    let output = std::process::Command::new("tmux")
        .arg("-V")
        .output()
        .unwrap();
    output.status.success()
        && agent_of_empires::tmux::tmux_pipe_input_version_supported(&String::from_utf8_lossy(
            &output.stdout,
        ))
}

fn tmux_field(h: &TuiTestHarness, target: &str, field: &str) -> Result<String, String> {
    let output = h
        .tmux()
        .args(["display-message", "-p", "-t", target, "-F", field])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn arm_result(gate: &std::path::Path, attempt: u64) -> Result<String, String> {
    std::fs::read_to_string(gate.join(format!("attempt-{attempt}-result")))
        .map_err(|error| error.to_string())
}

#[test]
#[parallel]
fn stale_pending_vt_arm_is_fenced_after_disconnect() {
    require_tmux!();
    if !tmux_supports_pipe_input() {
        eprintln!("Skipping test: tmux 3.8+ required for an input-capable pipe");
        return;
    }

    let mut h = TuiTestHarness::new_in_tmp("stale_pending_vt_arm");
    // The output-only clipboard observer also sets pane_pipe.
    h.append_config("[tmux]\nclipboard = \"disabled\"");
    let gate = h.home_path().join("vt-arm-gate");
    std::fs::create_dir(&gate).unwrap();
    h.stop_daemon_on_drop();
    let bin = h.install_path_command("claude");
    std::fs::write(
        bin.join("claude"),
        "#!/bin/sh\nprintf 'VT-ORIGINAL\\n'\nexec sleep 120\n",
    )
    .unwrap();

    let project = h.project_path();
    h.run_cli_ok(&["add", project.to_str().unwrap(), "-t", "VT Fence"]);
    let app = app_dir_in(h.home_path());
    let rows: Vec<agent_of_empires::session::Instance> =
        serde_json::from_slice(&std::fs::read(app.join("profiles/default/sessions.json")).unwrap())
            .unwrap();
    let row = rows.iter().find(|row| row.title == "VT Fence").unwrap();
    let session = agent_of_empires::tmux::Session::generate_name(&row.id, &row.title);
    let target = format!("={session}:^.0");

    h.spawn_tui();
    h.wait_for_runtime_ready();
    h.wait_for("VT Fence");
    h.send_keys("Tab");
    h.wait_for_timeout("LIVE", Duration::from_secs(10));
    wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        gate.join("attempt-1-held")
            .exists()
            .then_some(())
            .ok_or("first VT arm is not held".into())
    });
    assert_eq!(
        std::fs::read_to_string(gate.join("attempt-1-target")).unwrap(),
        session,
        "the gate held an unrelated preview target"
    );
    assert_eq!(tmux_field(&h, &target, "#{pane_pipe}").unwrap(), "0");
    let pane_id = tmux_field(&h, &target, "#{pane_id}").unwrap();
    let original_pid = tmux_field(&h, &target, "#{pane_pid}").unwrap();

    h.run_cli_ok(&["serve", "--stop"]);
    h.wait_for_timeout("Not responding", Duration::from_secs(10));
    wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        (std::fs::read_to_string(h.home_path().join("runtime-state"))
            .ok()
            .as_deref()
            == Some("busy"))
        .then_some(())
        .ok_or("runtime authority is not revoked".into())
    });

    let replacement = "/bin/sh -c 'printf \"VT-REPLACED\\n\"; exec sleep 120'";
    let output = h
        .tmux()
        .args(["respawn-pane", "-k", "-t", &pane_id, replacement])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "respawn-pane failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        let current_id = tmux_field(&h, &target, "#{pane_id}")?;
        let current_pid = tmux_field(&h, &target, "#{pane_pid}")?;
        let content =
            crate::live_send_paste_e2e::capture(&h.home_path().join("tmux.sock"), &session);
        (current_id == pane_id && current_pid != original_pid && content.contains("VT-REPLACED"))
            .then_some(())
            .ok_or("replacement process is not established in the original pane".into())
    });

    std::fs::write(gate.join("attempt-1-release"), b"release").unwrap();
    let first = wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        arm_result(&gate, 1)
    });
    assert_eq!(
        first, "none",
        "revoked pending arm installed an input-capable VT channel"
    );
    assert_eq!(tmux_field(&h, &target, "#{pane_pipe}").unwrap(), "0");
    wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        gate.join("attempt-1-settled")
            .exists()
            .then_some(())
            .ok_or("revoked pending arm did not settle".into())
    });
    assert!(
        !gate.join("attempt-2-target").exists(),
        "VT rearmed before explicit reconnect"
    );

    h.send_keys("r");
    h.wait_for_runtime_ready();
    h.wait_for_absent("Not responding", Duration::from_secs(10));
    let second = wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        arm_result(&gate, 2)
    });
    let owner = second
        .strip_prefix("input ")
        .unwrap_or_else(|| panic!("explicit reconnect did not acquire input: {second}"));
    wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        let pipe = tmux_field(&h, &target, "#{pane_pipe}")?;
        let installed_owner = tmux_field(&h, &target, "#{@aoe_vt_pipe_owner}")?;
        let adopted = std::fs::read_to_string(gate.join("adopted-owner")).ok();
        (adopted.as_deref() == Some(owner) && pipe != "0" && installed_owner == owner)
            .then_some(())
            .ok_or("reconnected VT input pipe is not installed and adopted".into())
    });
    assert_eq!(tmux_field(&h, &target, "#{pane_id}").unwrap(), pane_id);
    assert_ne!(
        tmux_field(&h, &target, "#{pane_pid}").unwrap(),
        original_pid
    );

    h.run_cli_ok(&["serve", "--stop"]);
    h.wait_for_timeout("Not responding", Duration::from_secs(10));
    wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
        (tmux_field(&h, &target, "#{pane_pipe}")? == "0")
            .then_some(())
            .ok_or("active VT input pipe survived revocation".into())
    });
}
