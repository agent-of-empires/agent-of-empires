//! Typing into an open agent terminal is direct user input, so it clears Ignore: a managed
//! row the operator is typing in cannot stay hidden at the Attention sort's sink.

use futures_util::{SinkExt, StreamExt};
use serial_test::parallel;
use std::time::{Duration, Instant};

use crate::harness::{app_dir_in, require_tmux, session_by_title, TuiTestHarness};

const INPUT_DEADLINE: Duration = Duration::from_secs(10);

#[test]
#[parallel]
fn typing_in_a_live_agent_terminal_clears_ignore() {
    use tokio_tungstenite::tungstenite::Message as Ws;

    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("live_ignore_input");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    // An agent that stays alive so its pane is capturable and takes keys.
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nsleep 300\n").expect("write sleeping agent");
    let project = h.project_path();
    let project = project.to_str().expect("utf8 project path").to_string();
    // The CLI drives the daemon, so it starts before any row is written.
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "serve failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let socket = app_dir_in(h.home_path()).join("daemon/api.sock");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let id = runtime.block_on(async {
        let http = reqwest::Client::builder()
            .unix_socket(socket.as_path())
            .build()
            .expect("build client");
        // `serve --daemon` returns once the child is spawned, so wait for its socket.
        let deadline = Instant::now() + INPUT_DEADLINE;
        while http.get("http://localhost/api/about").send().await.is_err() {
            assert!(Instant::now() < deadline, "the daemon never answered");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        h.run_cli_ok(&["add", &project, "-t", "Manager"]);
        let manager = session_by_title(&h.read_sessions(), "Manager")["id"]
            .as_str()
            .expect("manager id")
            .to_string();
        h.run_cli_ok(&["add", &project, "-t", "Managed", "--parent", &manager]);
        let id = session_by_title(&h.read_sessions(), "Managed")["id"]
            .as_str()
            .expect("managed id")
            .to_string();
        h.run_cli_ok(&["session", "start", &id]);
        let sessions_url = "http://localhost/api/sessions";
        async fn row_of(http: &reqwest::Client, url: &str, id: &str) -> serde_json::Value {
            let sessions: serde_json::Value = http
                .get(url)
                .send()
                .await
                .expect("list sessions")
                .error_for_status()
                .expect("list status")
                .json()
                .await
                .expect("list json");
            sessions["sessions"]
                .as_array()
                .expect("session list")
                .iter()
                .find(|row| row["id"] == id)
                .cloned()
                .unwrap_or_else(|| panic!("session {id} missing from the list"))
        }

        let ignored = http
            .patch(format!("http://localhost/api/sessions/{id}/ignored"))
            .json(&serde_json::json!({ "ignored": true }))
            .send()
            .await
            .expect("ignore request");
        let status = ignored.status();
        assert_eq!(
            status,
            reqwest::StatusCode::OK,
            "{}",
            ignored.text().await.unwrap()
        );
        let ignored_at = row_of(&http, sessions_url, &id).await["ignored_at"].clone();
        assert!(ignored_at.is_string(), "row was not ignored: {ignored_at}");

        let stream = tokio::net::UnixStream::connect(&socket)
            .await
            .expect("connect api.sock");
        let (mut ws, _) = tokio_tungstenite::client_async(
            format!("ws://localhost/sessions/{id}/live-ws"),
            stream,
        )
        .await
        .expect("live-ws upgrade");
        // The pane takes input only from its size owner, so claim it before typing.
        ws.send(Ws::Text(r#"{"type":"resize","cols":80,"rows":24}"#.into()))
            .await
            .expect("send resize");
        ws.send(Ws::Text(r#"{"type":"claim"}"#.into()))
            .await
            .expect("send claim");
        let owned = tokio::time::timeout(INPUT_DEADLINE, async {
            while let Some(Ok(message)) = ws.next().await {
                let Ws::Text(text) = message else {
                    continue;
                };
                let value: serde_json::Value = serde_json::from_str(&text).expect("control json");
                if value["type"] == "size_owner" && value["is_owner"] == true {
                    return;
                }
            }
        })
        .await;
        owned.expect("the claim never made this viewer the size owner");

        ws.send(Ws::Binary(b"x".to_vec().into()))
            .await
            .expect("send a keystroke");

        let deadline = Instant::now() + INPUT_DEADLINE;
        loop {
            let row = row_of(&http, sessions_url, &id).await;
            if row["ignored_at"].is_null() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "a keystroke left the row ignored: {row}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        id
    });
    assert_eq!(
        session_by_title(&h.read_sessions(), "Managed")["ignored_at"],
        serde_json::Value::Null,
        "typing in the open terminal left {id} ignored"
    );
}
