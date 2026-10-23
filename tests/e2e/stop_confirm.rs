//! Stop asks for confirmation that a second press of the stop key accepts.

use std::time::Duration;

use agent_of_empires::{
    daemon::{DaemonClient, StartSessionBody, TerminalTargetStatus},
    session::{AuxiliaryTarget, PanePresence, Status},
    tmux::{Session, TerminalSession, ToolSession},
};
use serial_test::parallel;

use crate::harness::{app_dir_in, require_tmux, wait_until, TuiTestHarness};

const TITLE: &str = "StopMe";

fn start_session(h: &TuiTestHarness) -> String {
    let project = h.project_path();
    let id = h.add_session(&[
        project.to_str().unwrap(),
        "-t",
        TITLE,
        "--cmd-override",
        "sleep 600",
    ]);
    h.run_cli_ok(&["session", "start", &id]);
    id
}

fn owned_pane(h: &TuiTestHarness, name: &str, kind: &str) -> String {
    let probe = h
        .tmux()
        .args([
            "display-message",
            "-t",
            name,
            "-p",
            "#{@aoe_kind}\t#{pane_id}",
        ])
        .output()
        .unwrap();
    assert!(probe.status.success(), "native pane {name} must exist");
    let identity = String::from_utf8(probe.stdout).unwrap();
    let (observed_kind, pane) = identity.trim().split_once('\t').unwrap();
    assert_eq!(observed_kind, kind, "pane must come from the native launch");
    pane.to_owned()
}

fn wait_gone(h: &TuiTestHarness, name: &str) {
    wait_until(Duration::from_secs(10), Duration::from_millis(100), || {
        if h.tmux_has_session(name) {
            Err(format!("{name} still running"))
        } else {
            Ok(())
        }
    });
}

/// Press the stop key, see the hint naming it, then press it again and watch the
/// pane die. A stray other key in between must not confirm.
fn stop_twice(h: &TuiTestHarness, stop_key: &str, dialog: &str, pane: &str) {
    h.send_keys(stop_key);
    h.wait_for(dialog);
    h.assert_screen_contains(&format!("Press {stop_key} again to confirm"));
    h.send_keys("j");
    h.assert_screen_contains(dialog);
    assert!(
        h.tmux_has_session(pane),
        "{pane} must survive the open confirm"
    );
    h.send_keys(stop_key);
    wait_gone(h, pane);
    // Wait for the applied receipt, not just physical teardown.
    h.wait_for("Runtime ready");
}

#[tokio::test]
#[parallel]
async fn second_x_confirms_stop_in_terminal_and_agent_views() {
    require_tmux!();
    let mut h = TuiTestHarness::new("stop_confirm");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let id = start_session(&h);
    let agent = Session::generate_name(&id, TITLE);
    let agent_pane = owned_pane(&h, &agent, "agent");
    let terminal = TerminalSession::generate_name(&id, TITLE);

    h.run_cli_ok(&["serve", "--core-only", "--daemon"]);
    let sdk = DaemonClient::new_unix(app_dir_in(h.home_path()).join("daemon/api.sock")).unwrap();
    let epoch = sdk.runtime_info().await.unwrap().epoch;
    let receipt = sdk
        .ensure_terminal(&id, 0, &StartSessionBody::default(), &epoch)
        .await
        .unwrap();
    assert!(matches!(
        receipt.outcome.status,
        TerminalTargetStatus::Created
    ));
    assert_eq!(receipt.outcome.tmux_session, terminal);
    assert_eq!(receipt.outcome.profile, "default");
    assert_eq!(receipt.cursor.epoch, epoch);
    owned_pane(&h, &terminal, "term");

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for(TITLE);
    h.wait_for("Runtime ready");

    h.send_keys("t");
    stop_twice(&h, "x", "Kill Terminal", &terminal);
    h.wait_for("Press Enter to start terminal");
    assert_eq!(
        owned_pane(&h, &agent, "agent"),
        agent_pane,
        "killing the terminal must preserve the exact launched agent pane"
    );
    let rows = sdk.list_sessions(None).await.unwrap().sessions;
    let row = rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(row.agent_pane.state, PanePresence::Alive);
    assert_eq!(row.agent_pane.tmux_session.as_deref(), Some(agent.as_str()));
    assert!(row.auxiliary.iter().any(|observation| {
        observation.target == AuxiliaryTarget::Host { index: 0 }
            && observation.pane.state == PanePresence::Absent
    }));

    h.send_keys("t");
    stop_twice(&h, "x", "Stop Session", &agent);
    let rows = sdk.list_sessions(None).await.unwrap().sessions;
    let row = rows.iter().find(|row| row.id == id).unwrap();
    assert_eq!(row.status, "Stopped");
    assert_eq!(row.agent_pane.state, PanePresence::Absent);
    let committed: Vec<agent_of_empires::session::Instance> = serde_json::from_slice(
        &std::fs::read(app_dir_in(h.home_path()).join("profiles/default/sessions.json")).unwrap(),
    )
    .unwrap();
    let row = committed.iter().find(|row| row.id == id).unwrap();
    assert_eq!(row.status, Status::Stopped);
    assert!(row.lifecycle_reservation.is_none());
    let panes = h
        .tmux()
        .args(["list-panes", "-a", "-F", "#{pane_id}"])
        .output()
        .unwrap();
    assert!(panes.status.success());
    assert!(!String::from_utf8_lossy(&panes.stdout)
        .lines()
        .any(|pane| pane == agent_pane));
}

#[test]
#[parallel]
fn second_shift_x_confirms_tool_kill_in_strict_mode() {
    require_tmux!();
    let mut h = TuiTestHarness::new("stop_confirm_strict");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.append_config(
        "[session]\nstrict_hotkeys = true\n\n[tools.idletool]\ncommand = \"sleep 600\"\nhotkey = \"Alt+t\"",
    );
    let id = start_session(&h);
    let agent = Session::generate_name(&id, TITLE);
    let tool = ToolSession::new(&id, TITLE, "idletool")
        .session_name()
        .to_string();
    h.tmux_new_detached(&tool, "sleep 600");

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for(TITLE);

    h.send_keys("M-t");
    h.wait_for("Tool: idletool");
    h.wait_for("Adopt Legacy Tool");
    h.send_keys("Escape");
    let probe = h
        .tmux()
        .args(["display-message", "-t", &tool, "-p", "#{@aoe_tool_owner}"])
        .output()
        .unwrap();
    assert!(probe.status.success());
    assert!(
        String::from_utf8_lossy(&probe.stdout).trim().is_empty(),
        "fixture remains deliberately unmarked"
    );
    h.send_keys("X");
    h.wait_for("Kill Tool");
    h.assert_screen_contains("Adopt and kill legacy tool");
    h.assert_screen_contains(&id);
    h.assert_screen_contains(&tool);
    h.assert_screen_contains("Press X again to confirm");
    h.send_keys("j");
    h.assert_screen_contains("Kill Tool");
    assert!(h.tmux_has_session(&tool));
    let probe = h
        .tmux()
        .args(["display-message", "-t", &tool, "-p", "#{@aoe_tool_owner}"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&probe.stdout).trim().is_empty(),
        "opening confirmation never stamps ownership"
    );
    h.send_keys("X");
    wait_gone(&h, &tool);
    assert!(
        h.tmux_has_session(&agent),
        "killing the tool must not stop the agent"
    );
}
