//! `aoe send` against an ACP/structured-view session has no tmux pane to type
//! into, so it must dispatch through the running daemon's prompt endpoint
//! instead of bailing with "no tmux pane".

use std::time::Duration;

use serial_test::parallel;

use crate::harness::{app_dir_in, require_node, require_tmux, wait_until, TuiTestHarness};

#[test]
#[parallel]
fn send_delivers_to_structured_session_via_daemon() {
    require_tmux!();
    require_node!();
    let h = TuiTestHarness::new_acp(
        "send_structured",
        r#"{ "turns": [
            { "updates": [], "stopReason": "end_turn" },
            { "updates": [], "stopReason": "end_turn" }
        ] }"#,
    );
    let (_, session_id) = h.start_structured_session("send_structured");
    h.prompt_until_accepted(&session_id, "warm up", Duration::from_secs(30));

    let out = h.run_cli_ok(&["send", &session_id, "hello via send"]);
    assert!(
        out.contains("message to 'send_structured'"),
        "unexpected send output: {out}"
    );

    // The CLI's acknowledgement alone doesn't prove the message reached the
    // agent; confirm the fake ACP agent actually received it. The daemon's
    // 202 response races the worker's own log write, so poll briefly.
    let log_path = app_dir_in(h.home_path()).join("fake-acp.log");
    wait_until(Duration::from_secs(5), Duration::from_millis(50), || {
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        log.contains("hello via send")
            .then_some(())
            .ok_or_else(|| format!("fake ACP did not receive the sent message: {log}"))
    });
}
