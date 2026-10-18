//! A real TUI beside a real `aoe serve` on one profile: a structured row's
//! sidebar glyph follows the status the daemon reports (#3841).

use std::time::Duration;

use serde_json::Value;
use serial_test::parallel;

use crate::harness::{require_node, require_tmux, wait_until, TuiTestHarness};

/// `spinners::dots()` frames, which paint a Running structured row.
const RUNNING_FRAMES: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏";
/// `ICON_IDLE`.
const IDLE_ICON: char = '⠒';

const HELD_TURN_SCRIPT: &str = r#"{
  "turns": [
    {
      "updates": [
        { "sessionUpdate": "wait_for_release" },
        {
          "sessionUpdate": "agent_message_chunk",
          "content": { "type": "text", "text": "done" }
        }
      ],
      "stopReason": "end_turn"
    }
  ]
}"#;

fn daemon_status(port: u16, session_id: &str) -> Result<String, String> {
    let body: Value = tokio::runtime::Runtime::new()
        .expect("tokio runtime")
        .block_on(async {
            reqwest::get(format!("http://127.0.0.1:{port}/api/sessions"))
                .await?
                .json()
                .await
        })
        .map_err(|e| format!("GET /api/sessions: {e}"))?;
    body["sessions"]
        .as_array()
        .and_then(|rows| rows.iter().find(|r| r["id"] == session_id))
        .and_then(|row| row["status"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("session {session_id} missing from {body}"))
}

fn wait_for_daemon_status(port: u16, session_id: &str, want: &str) {
    wait_until(Duration::from_secs(20), Duration::from_millis(200), || {
        let got = daemon_status(port, session_id)?;
        got.eq_ignore_ascii_case(want)
            .then_some(())
            .ok_or_else(|| format!("daemon status is {got:?}, want {want:?}"))
    });
}

/// The glyph on the sidebar row carrying `title`.
fn row_glyph(h: &TuiTestHarness, title: &str) -> Option<char> {
    h.capture_screen()
        .lines()
        .find(|line| line.contains(title) && !line.contains("Title:"))
        .and_then(|line| {
            line.chars()
                .find(|c| *c == IDLE_ICON || RUNNING_FRAMES.contains(*c))
        })
}

fn wait_for_row_glyph(h: &TuiTestHarness, title: &str, want: impl Fn(char) -> bool, what: &str) {
    wait_until(
        Duration::from_secs(20),
        Duration::from_millis(200),
        || match row_glyph(h, title) {
            Some(c) if want(c) => Ok(()),
            got => Err(format!(
                "row glyph is {got:?}, want {what}.\nscreen:\n{}",
                h.capture_screen()
            )),
        },
    );
}

#[test]
#[parallel]
fn sidebar_structured_status_follows_the_daemon() {
    require_tmux!();
    require_node!();
    let mut h = TuiTestHarness::new_acp("tui_daemon_sidebar", HELD_TURN_SCRIPT);
    let (port, session_id) = h.start_structured_session("sidebar_follow");
    h.prompt_until_accepted(&session_id, "hold the turn", Duration::from_secs(30));
    wait_for_daemon_status(port, &session_id, "running");

    h.spawn_tui();
    h.wait_for(" aoe ");
    h.wait_for("sidebar_follow");
    wait_for_row_glyph(
        &h,
        "sidebar_follow",
        |c| RUNNING_FRAMES.contains(c),
        "a running spinner frame",
    );

    std::fs::write(h.home_path().join("fake-acp-script.json.release"), "")
        .expect("release the held turn");
    wait_for_daemon_status(port, &session_id, "idle");
    wait_for_row_glyph(&h, "sidebar_follow", |c| c == IDLE_ICON, "the idle glyph");
}
