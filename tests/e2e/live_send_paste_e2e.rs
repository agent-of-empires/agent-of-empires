//! e2e coverage for multi-line paste in live-send mode.
//!
//! aoe used to hand-roll the xterm bracketed-paste markers (`ESC[200~` /
//! `ESC[201~`) into a raw `send-keys -H` payload, so every pane received them
//! whether or not it had set DECSET 2004. A raw shell or a SQL REPL parses
//! `ESC[2` as a partial Insert-key sequence, discards it on the non-matching
//! next byte, and self-inserts the leftover `00~` / `01~` into the user's
//! text. Pasting the same query after fully entering the view worked, because
//! tmux's own paste path only emits the markers when the program asked for
//! them.
//!
//! This drives the real binary from a real bracketed paste at the TUI's stdin
//! through `handle_paste` -> live-send worker -> tmux, and asserts the agent
//! pane received the query with no marker debris. Unit tests cover the payload
//! shape; only an e2e can prove the bytes that land in the pane.

use serial_test::parallel;
use std::process::Command;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// Route activation straight into live-send, mirroring `live_takeover`.
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

fn agent_session_on(socket: &std::path::Path) -> String {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
        .expect("tmux list-sessions");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|name| name.starts_with("aoe_dev_") && !name.starts_with("aoe_e2e_"))
        .map(String::from)
        .expect("agent tmux session exists")
}

fn capture(socket: &std::path::Path, session: &str) -> String {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["capture-pane", "-t", session, "-p"])
        .output()
        .expect("tmux capture-pane");
    String::from_utf8_lossy(&output.stdout).to_string()
}

/// A multi-line paste into a pane whose program never set DECSET 2004 must
/// arrive as plain text. `cat -v` renders any escape byte visibly (`^[[200~`),
/// so marker debris cannot hide inside the captured pane; a plain `cat` would
/// let the terminal swallow the sequence and the regression would pass.
#[test]
#[parallel]
fn test_live_send_paste_into_non_bracketed_pane_has_no_marker_debris() {
    require_tmux!();

    let mut h = TuiTestHarness::new("live_send_paste");
    write_live_send_config(&h);

    // Stand in for a raw shell / SQL REPL: echoes what it receives, never
    // enables bracketed paste, and outlives the test.
    let bin = h.install_path_command("claude");
    std::fs::write(
        bin.join("claude"),
        // `-icanon` matters: in canonical mode the tty buffers input until a
        // complete line, so a paste with no trailing newline would strand its
        // last line in the line discipline and never reach the program.
        "#!/bin/sh\nstty -echo -icanon min 1 time 0 2>/dev/null\nexec cat -v\n",
    )
    .expect("write cat stub");

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Pasty"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    h.wait_for("Pasty");
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(10));

    h.send_paste("SELECT id\nFROM users;");

    // Poll rather than sleep a fixed span: a paste crosses the TUI, the
    // live-send worker, a tmux fork, and the pane program, and any fixed
    // budget is either flaky on a loaded box or slow on an idle one. The
    // marker assertion below still holds on the buggy build, because the
    // debris arrives in the same write as the text it brackets.
    let socket = h.home_path().join("tmux.sock");
    let session = agent_session_on(&socket);
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let pane = loop {
        let pane = capture(&socket, &session);
        if pane.contains("SELECT id") && pane.contains("FROM users") {
            break pane;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "pasted query never reached the agent pane, last capture:\n{pane}"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(
        !pane.contains("200~") && !pane.contains("201~"),
        "bracketed-paste markers must not reach a pane that never set \
         DECSET 2004 (this is the `00~` / `01~` bug), got:\n{pane}"
    );
}
