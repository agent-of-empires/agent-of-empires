//! The built-in attention/triage worker (`aoe __plugin-attention`).
//!
//! Answers "who needs me?" across a fleet of sessions: it polls the host's
//! `sessions.list` on a timer, groups the sessions by urgency, and pushes a
//! home-pane board plus a status-bar count of sessions waiting on input. It
//! runs as `aoe` re-invoked through the self-exec runtime and reads the session
//! list over the plugin protocol (the `session.read` capability), the same way
//! a third-party plugin would.

use std::io::{BufRead, Write};
use std::time::Duration;

use serde_json::{json, Value};

/// Seconds between polls. Session states change on human timescales, so a few
/// seconds keeps the board current without hammering the host.
const REFRESH_SECS: u64 = 3;

/// Cap on session rows in the board. The whole board is one home-pane entry
/// under a 64 KiB payload ceiling; on a very large fleet an uncapped board
/// would exceed it and the push would be silently rejected (it is sent as a
/// notification, so the worker sees no error). Rows fill in urgency order, so
/// the cap keeps the most urgent visible and a trailing note reports the rest.
const MAX_BOARD_ROWS: usize = 100;

/// Buckets in urgency order: `(status wire name, section heading)`. The status
/// strings are `Status::wire_str` values, which is what `sessions.list`
/// serializes. Idle ranks above Running (an idle agent may want the next step;
/// a running one does not), matching the app's own attention-sort ranking.
/// Transient states (Starting/Creating/Deleting/Stopped/Unknown) are
/// intentionally omitted; they are not something the operator acts on.
const BUCKETS: [(&str, &str); 4] = [
    ("Waiting", "Needs input"),
    ("Error", "Errored"),
    ("Idle", "Idle"),
    ("Running", "Running"),
];

/// The outcome of one `sessions.list` poll.
enum Poll {
    /// The session list from the host.
    Sessions(Vec<Value>),
    /// The host answered with an error rather than a list; leave the prior
    /// board in place rather than flashing "no sessions" over a transient
    /// hiccup (this is a "who needs me" view, so hiding a waiting session,
    /// even for one poll, is the wrong failure).
    HostError,
    /// EOF: the host closed the pipe.
    Closed,
}

/// Run the worker loop until the host closes the pipe.
pub fn run() -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    let mut reader = stdin.lock();
    let mut out = std::io::stdout();
    let mut request_id: u64 = 0;
    loop {
        request_id += 1;
        let request = json!({
            "jsonrpc": "2.0",
            "id": request_id,
            "method": "sessions.list",
            "params": {"exclude": ["trashed", "archived"]},
        });
        if writeln!(out, "{request}").is_err() || out.flush().is_err() {
            break;
        }

        match poll_sessions(&mut reader, request_id) {
            Poll::Closed => break,
            Poll::HostError => {}
            Poll::Sessions(sessions) => {
                let (board, waiting) = build_board(&sessions);
                let board_line = ui_state_set_line("home-pane", "board", board);
                let count_line = ui_state_set_line("status-bar", "waiting", waiting);
                if writeln!(out, "{board_line}").is_err()
                    || writeln!(out, "{count_line}").is_err()
                    || out.flush().is_err()
                {
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(REFRESH_SECS));
    }
    Ok(())
}

/// Read host lines until the response to `request_id` arrives. Other lines
/// (unrelated notifications) are skipped.
fn poll_sessions(reader: &mut impl BufRead, request_id: u64) -> Poll {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return Poll::Closed,
            Ok(_) => {}
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if msg["id"].as_u64() != Some(request_id) {
            continue;
        }
        if !msg["error"].is_null() {
            return Poll::HostError;
        }
        return Poll::Sessions(
            msg["result"]["sessions"]
                .as_array()
                .cloned()
                .unwrap_or_default(),
        );
    }
}

/// Build the home-pane board (one heading + row per bucketed session) and the
/// status-bar payload, returning the payloads and, via the status-bar text, the
/// count of sessions waiting on input.
fn build_board(sessions: &[Value]) -> (Value, Value) {
    let waiting = sessions.iter().filter(|s| s["status"] == "Waiting").count();
    let mut blocks = Vec::new();
    let mut shown = 0usize;
    let mut omitted = 0usize;
    for (status, heading) in BUCKETS {
        let rows: Vec<&Value> = sessions.iter().filter(|s| s["status"] == status).collect();
        if rows.is_empty() {
            continue;
        }
        // The heading always reports the true bucket size; rows below it are
        // subject to the shared MAX_BOARD_ROWS budget.
        blocks.push(json!({"kind": "heading", "text": format!("{heading} ({})", rows.len())}));
        for session in rows {
            if shown >= MAX_BOARD_ROWS {
                omitted += 1;
                continue;
            }
            let title = session["title"].as_str().unwrap_or("session");
            let project = session["project_path"].as_str().unwrap_or("");
            blocks.push(json!({"kind": "row", "label": title, "sublabel": project}));
            shown += 1;
        }
    }
    if omitted > 0 {
        blocks.push(json!({"kind": "note", "text": format!("+{omitted} more not shown")}));
    }
    if blocks.is_empty() {
        blocks.push(json!({"kind": "note", "text": "No active sessions."}));
    }

    let board = json!({"title": "Attention", "blocks": blocks});
    // Empty text clears the segment when nothing is waiting (the renderer drops
    // a blank status-bar entry), so a stale count never lingers.
    let status_bar = if waiting > 0 {
        json!({"text": format!("{waiting} waiting"), "tone": "warn"})
    } else {
        json!({"text": ""})
    };
    (board, status_bar)
}

/// A `ui.state.set` JSON-RPC notification line (no `id`: the host runs it and
/// sends nothing back).
fn ui_state_set_line(slot: &str, id: &str, payload: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "method": "ui.state.set",
        "params": {"slot": slot, "id": id, "payload": payload},
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, status: &str) -> Value {
        json!({"id": id, "title": format!("s-{id}"), "project_path": "/repo", "status": status})
    }

    #[test]
    fn board_groups_by_urgency_and_counts_waiting() {
        let sessions = [
            session("a", "Waiting"),
            session("b", "Waiting"),
            session("c", "Error"),
            session("d", "Running"),
            session("f", "Idle"),
            session("e", "Starting"), // transient: not bucketed
        ];
        let (board, status_bar) = build_board(&sessions);
        let blocks = board["blocks"].as_array().unwrap();

        // Headings in urgency order (Idle above Running), each with its count;
        // the transient "Starting" session contributes no bucket.
        let headings: Vec<&str> = blocks
            .iter()
            .filter(|b| b["kind"] == "heading")
            .map(|b| b["text"].as_str().unwrap())
            .collect();
        assert_eq!(
            headings,
            ["Needs input (2)", "Errored (1)", "Idle (1)", "Running (1)"]
        );

        // Two waiting rows follow the first heading.
        assert_eq!(blocks[0]["text"], "Needs input (2)");
        assert_eq!(blocks[1]["kind"], "row");
        assert_eq!(blocks[1]["sublabel"], "/repo");

        assert_eq!(status_bar["text"], "2 waiting");
        assert_eq!(status_bar["tone"], "warn");
    }

    #[test]
    fn empty_board_notes_no_sessions_and_clears_the_count() {
        let (board, status_bar) = build_board(&[]);
        let blocks = board["blocks"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["kind"], "note");
        // No one waiting: the status-bar text is blank so the segment clears.
        assert_eq!(status_bar["text"], "");
    }

    #[test]
    fn count_clears_when_nothing_waiting_but_others_active() {
        let (_, status_bar) = build_board(&[session("a", "Running"), session("b", "Idle")]);
        assert_eq!(status_bar["text"], "");
    }

    #[test]
    fn board_caps_rows_and_reports_the_remainder() {
        let sessions: Vec<Value> = (0..MAX_BOARD_ROWS + 25)
            .map(|i| session(&i.to_string(), "Running"))
            .collect();
        let (board, _) = build_board(&sessions);
        let blocks = board["blocks"].as_array().unwrap();
        let rows = blocks.iter().filter(|b| b["kind"] == "row").count();
        assert_eq!(rows, MAX_BOARD_ROWS);
        assert!(
            blocks
                .iter()
                .any(|b| b["kind"] == "note" && b["text"] == "+25 more not shown"),
            "a trailing note reports the omitted rows: {blocks:?}"
        );
    }

    #[test]
    fn poll_reads_the_matching_response_and_skips_unrelated_lines() {
        let mut input: &[u8] = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"some.notification\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"sessions\":[{\"id\":\"a\",\"status\":\"Waiting\"}]}}\n",
        )
        .as_bytes();
        match poll_sessions(&mut input, 1) {
            Poll::Sessions(sessions) => assert_eq!(sessions.len(), 1),
            _ => panic!("expected Sessions"),
        }
    }

    #[test]
    fn poll_reports_host_error_rather_than_an_empty_list() {
        let mut input: &[u8] =
            b"{\"jsonrpc\":\"2.0\",\"id\":1,\"error\":{\"code\":-32000,\"message\":\"boom\"}}\n";
        assert!(matches!(poll_sessions(&mut input, 1), Poll::HostError));
    }

    #[test]
    fn poll_reports_closed_on_eof() {
        let mut input: &[u8] = b"";
        assert!(matches!(poll_sessions(&mut input, 1), Poll::Closed));
    }
}
