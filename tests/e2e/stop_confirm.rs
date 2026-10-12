//! Stop asks for confirmation that a second press of the stop key accepts.

use std::time::Duration;

use agent_of_empires::tmux::{Session, TerminalSession, ToolSession};
use serial_test::parallel;

use crate::harness::{parse_session_id, require_tmux, wait_until, TuiTestHarness};

const TITLE: &str = "StopMe";

fn add_session(h: &TuiTestHarness) -> String {
    let project = h.project_path();
    parse_session_id(&h.run_cli_ok(&["add", project.to_str().unwrap(), "-t", TITLE]))
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
}

#[test]
#[parallel]
fn second_x_confirms_stop_in_terminal_and_agent_views() {
    require_tmux!();
    let mut h = TuiTestHarness::new("stop_confirm");
    let id = add_session(&h);
    let agent = Session::generate_name(&id, TITLE);
    let terminal = TerminalSession::generate_name(&id, TITLE);
    h.tmux_new_detached(&agent, "sleep 600");
    h.tmux_new_detached(&terminal, "sleep 600");

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for(TITLE);

    h.send_keys("t");
    stop_twice(&h, "x", "Kill Terminal", &terminal);
    assert!(
        h.tmux_has_session(&agent),
        "killing the terminal must not stop the agent"
    );

    h.send_keys("t");
    stop_twice(&h, "x", "Stop Session", &agent);
}

#[test]
#[parallel]
fn second_shift_x_confirms_tool_kill_in_strict_mode() {
    require_tmux!();
    let mut h = TuiTestHarness::new("stop_confirm_strict");
    h.append_config(
        "[session]\nstrict_hotkeys = true\n\n[tools.idletool]\ncommand = \"sleep 600\"\nhotkey = \"Alt+t\"",
    );
    let id = add_session(&h);
    let agent = Session::generate_name(&id, TITLE);
    let tool = ToolSession::new(&id, TITLE, "idletool")
        .session_name()
        .to_string();
    h.tmux_new_detached(&agent, "sleep 600");
    h.tmux_new_detached(&tool, "sleep 600");

    h.spawn_tui();
    h.wait_for_ready();
    h.wait_for(TITLE);

    h.send_keys("M-t");
    h.wait_for("Tool: idletool");
    stop_twice(&h, "X", "Kill Tool", &tool);
    assert!(
        h.tmux_has_session(&agent),
        "killing the tool must not stop the agent"
    );
}
