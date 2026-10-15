//! e2e coverage for the live-mode leader `t`: keystrokes move between the agent pane and
//! its paired terminal without leaving live mode. Only an e2e proves which tmux pane the
//! typed bytes land in.

use serial_test::parallel;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::harness::{require_tmux, TuiTestHarness};
use crate::live_send_paste_e2e::{capture, write_live_send_config};

fn session_with_prefix(socket: &std::path::Path, prefix: &str) -> Option<String> {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .expect("tmux list-sessions");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|name| name.starts_with(prefix))
        .map(String::from)
}

fn wait_for_pane_text(socket: &std::path::Path, prefix: &str, text: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(session) = session_with_prefix(socket, prefix) {
            let pane = capture(socket, &session);
            if pane.contains(text) {
                return pane;
            }
        }
        assert!(
            Instant::now() < deadline,
            "{text:?} never reached a {prefix}* pane"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
#[parallel]
fn test_leader_t_moves_live_keys_between_agent_and_terminal() {
    require_tmux!();

    let mut h = TuiTestHarness::new("live_terminal_swap");
    write_live_send_config(&h);
    let bin = h.install_path_command("claude");
    std::fs::write(
        bin.join("claude"),
        "#!/bin/sh\nstty -echo -icanon min 1 time 0 2>/dev/null\nexec cat -v\n",
    )
    .expect("write cat stub");

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Swappy"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("Swappy");
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(10));
    let socket = h.home_path().join("tmux.sock");

    h.send_keys("C-b");
    h.send_keys("t");
    h.wait_for_timeout("(terminal)", Duration::from_secs(10));
    h.send_keys("echo SHELL_SIDE");
    h.send_keys("Enter");
    wait_for_pane_text(&socket, "aoe_dev_term_", "SHELL_SIDE");

    h.send_keys("C-b");
    h.send_keys("t");
    h.send_keys("AGENT_SIDE");
    let agent = wait_for_pane_text(&socket, "aoe_dev_Swappy", "AGENT_SIDE");
    assert!(
        !agent.contains("SHELL_SIDE"),
        "shell keystrokes must not reach the agent pane:\n{agent}"
    );
    assert!(
        h.capture_screen().contains("LIVE"),
        "swapping must not leave live mode"
    );
}
