//! E2E for the live-send takeover exit: when another surface steals the
//! size-owner lock (what the web dashboard's "take over" does), the TUI
//! must exit live mode on its own, without a keystroke, and explain who
//! took over. Guards against the old silent tug-of-war where the TUI
//! re-stole the lock on the next input batch and reverted the web's grid.

use serial_test::parallel;
use std::process::Command;
use std::time::Duration;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// Replace the pre-seeded config with one that routes activation into
/// live-send (mirrors `new_session::write_config_attach_mode_live_send`).
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

{systemd_off}"#,
        version = env!("CARGO_PKG_VERSION"),
        systemd_off = crate::harness::SYSTEMD_OFF,
    );
    std::fs::write(config_dir.join("config.toml"), config_content).expect("write live-send config");
}

fn list_sessions_on(socket: &std::path::Path) -> Vec<String> {
    let output = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["list-sessions", "-F", "#{session_name}"])
        .output();
    match output {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    }
}

fn set_session_option(socket: &std::path::Path, session: &str, opt: &str, value: &str) {
    let out = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["set-option", "-t", session, opt, value])
        .output()
        .expect("tmux set-option");
    assert!(
        out.status.success(),
        "tmux set-option {opt} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
#[parallel]
fn test_web_takeover_exits_tui_live_mode_with_dialog() {
    require_tmux!();

    let mut h = TuiTestHarness::new("live_takeover");
    write_live_send_config(&h);

    // The default `claude` stub exits immediately, which kills the agent
    // pane before live mode can observe anything (the known short-lived-
    // shell flake). Shadow it with a long-running stand-in so the pane
    // survives the whole test.
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nsleep 300\n").expect("write sleeping claude");

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Takeover"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    h.spawn_tui();
    // The native pane preparation refuses an unhealthy runtime, so wait for the
    // subscription to report readiness before driving panes.
    h.wait_for_runtime_ready();
    h.wait_for("Takeover");

    // Activate the session; default_attach_mode routes into live-send.
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", Duration::from_secs(10));

    // Find the agent's tmux session on the harness socket (the harness's
    // own UI session is `aoe_e2e_*`; agent sessions carry the dev prefix).
    let socket = h.home_path().join("tmux.sock");
    let agent_session = list_sessions_on(&socket)
        .into_iter()
        .find(|name| name.starts_with("aoe_dev_") && !name.starts_with("aoe_e2e_"))
        .expect("agent tmux session exists");

    // Steal the size-owner lock the way the web dashboard's Claim handler
    // does (`steal_size_owner`): overwrite owner + fresh heartbeat with a
    // `live-*` id.
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_millis()
        .to_string();
    set_session_option(&socket, &agent_session, "@aoe_size_owner", "live-e2e-web");
    set_session_option(&socket, &agent_session, "@aoe_size_owner_hb", &now_ms);

    // With NO further input, the worker's idle heartbeat notices the loss
    // (<= 1.5s), the main-loop poll exits live mode, and the dialog names
    // the web dashboard. The LIVE badge must be gone: live mode ended.
    h.wait_for_timeout("Live send ended", Duration::from_secs(10));
    h.assert_screen_contains("web dashboard");
    h.wait_for_absent("LIVE", Duration::from_secs(5));

    // The thief keeps the lock: the TUI must not have stolen it back.
    let owner = Command::new("tmux")
        .arg("-S")
        .arg(&socket)
        .args([
            "show-options",
            "-v",
            "-t",
            &agent_session,
            "@aoe_size_owner",
        ])
        .output()
        .expect("tmux show-options");
    assert_eq!(
        String::from_utf8_lossy(&owner.stdout).trim(),
        "live-e2e-web",
        "TUI must not re-steal the size-owner lock after takeover"
    );
}

fn show_option(socket: &std::path::Path, session: &str, opt: &str) -> String {
    let out = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(["show-options", "-qv", "-t", session, opt])
        .output()
        .expect("tmux show-options");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A Web tab that opens while the TUI is in live mode falls back to capture frames, since
/// the TUI's preview holds the pane's VT pipe. Taking control must make the real TUI give
/// the pipe up, so the same socket moves onto the grid.
#[test]
#[parallel]
fn test_web_claim_moves_the_socket_onto_the_tuis_released_pipe() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message as Ws;

    require_tmux!();

    let mut h = TuiTestHarness::new("live_takeover_pipe");
    write_live_send_config(&h);
    let bin = h.install_path_command("claude");
    std::fs::write(bin.join("claude"), "#!/bin/sh\nsleep 300\n").expect("write sleeping claude");

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "Takeover"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let rows: Vec<serde_json::Value> =
        serde_json::from_str(&h.run_cli_ok(&["list", "--json"])).expect("list json");
    let id = rows
        .iter()
        .find(|row| row["title"] == "Takeover")
        .and_then(|row| row["id"].as_str())
        .expect("session id")
        .to_string();

    // A deadline only bounds failure, so the startup waits allow a cold start after a
    // build. The claim-to-grid wait below is bound by the daemon's re-arm window instead.
    const STARTUP: Duration = Duration::from_secs(30);
    h.spawn_tui();
    let runtime_state = h.home_path().join("runtime-state");
    crate::harness::wait_until(
        STARTUP,
        Duration::from_millis(100),
        || match std::fs::read_to_string(&runtime_state) {
            Ok(state) if state == "ready" => Ok(()),
            last => Err(format!("runtime-state is {last:?}")),
        },
    );
    h.wait_for_timeout("Takeover", STARTUP);
    h.send_keys("Enter");
    h.wait_for_timeout("LIVE", STARTUP);

    let socket = h.home_path().join("tmux.sock");
    let agent_session = list_sessions_on(&socket)
        .into_iter()
        .find(|name| name.starts_with("aoe_dev_") && !name.starts_with("aoe_e2e_"))
        .expect("agent tmux session exists");
    let tui_pipe = crate::harness::wait_until(STARTUP, Duration::from_millis(50), || {
        let owner = show_option(&socket, &agent_session, "@aoe_vt_owner");
        if owner.is_empty() {
            Err("the TUI preview never armed its VT pipe".to_string())
        } else {
            Ok(owner)
        }
    });
    // The live worker takes the size lock on its own thread, so wait for it too: a Web
    // resize on a vacant lock would claim it.
    crate::harness::wait_until(STARTUP, Duration::from_millis(50), || {
        let owner = show_option(&socket, &agent_session, "@aoe_size_owner");
        if owner.starts_with("tui-") {
            Ok(())
        } else {
            Err(format!(
                "the TUI never took the size lock; holder is {owner:?}"
            ))
        }
    });

    let api = app_dir_in(h.home_path()).join("daemon/api.sock");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let stream = tokio::net::UnixStream::connect(&api)
            .await
            .expect("connect api.sock");
        let (mut ws, _) = tokio_tungstenite::client_async(
            format!("ws://localhost/sessions/{id}/live-ws"),
            stream,
        )
        .await
        .expect("live-ws upgrade");
        async fn until<S>(
            ws: &mut S,
            timeout: Duration,
            what: &str,
            mut done: impl FnMut(&serde_json::Value) -> bool,
        ) -> serde_json::Value
        where
            S: futures_util::Stream<Item = Result<Ws, tokio_tungstenite::tungstenite::Error>>
                + Unpin,
        {
            tokio::time::timeout(timeout, async {
                loop {
                    match ws.next().await {
                        Some(Ok(Ws::Text(text))) => {
                            let value: serde_json::Value = serde_json::from_str(&text).unwrap();
                            if done(&value) {
                                return value;
                            }
                        }
                        Some(Ok(_)) => {}
                        other => panic!("socket ended waiting for {what}: {other:?}"),
                    }
                }
            })
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        }

        ws.send(Ws::Text(r#"{"type":"resize","cols":80,"rows":24}"#.into()))
            .await
            .unwrap();
        // The capture loop announces the transport on its own task, so it may arrive before
        // or after the resize's size_owner reply.
        let (mut owner, mut fallback) = (None, None);
        until(
            &mut ws,
            Duration::from_secs(10),
            "size_owner and transport",
            |v| {
                match v["type"].as_str() {
                    Some("size_owner") if owner.is_none() => owner = Some(v.clone()),
                    Some("transport") if fallback.is_none() => fallback = Some(v.clone()),
                    _ => {}
                }
                owner.is_some() && fallback.is_some()
            },
        )
        .await;
        let (owner, fallback) = (owner.unwrap(), fallback.unwrap());
        assert_eq!(owner["is_owner"], false, "{owner}");
        assert_eq!(owner["owner"], "tui", "{owner}");
        assert_eq!(fallback["grid"], false, "{fallback}");
        assert_eq!(fallback["reason"], "pipe_owned", "{fallback}");

        ws.send(Ws::Text(r#"{"type":"claim"}"#.into()))
            .await
            .unwrap();
        // The claim queues the re-arm before it replies, so the grid transport can
        // arrive first. The daemon retries arming for twice the VT lease TTL.
        let (mut claimed, mut grid) = (false, None);
        until(
            &mut ws,
            Duration::from_secs(10),
            "claimed and grid transport",
            |v| {
                match v["type"].as_str() {
                    Some("size_owner") if v["is_owner"] == true => claimed = true,
                    Some("transport") if grid.is_none() => grid = Some(v.clone()),
                    _ => {}
                }
                claimed && grid.is_some()
            },
        )
        .await;
        let grid = grid.unwrap();
        assert_eq!(grid["grid"], true, "{grid}");
        // Read while the socket is open: closing it releases the daemon's pipe.
        let owner = show_option(&socket, &agent_session, "@aoe_vt_owner");
        assert!(
            !owner.is_empty() && owner != tui_pipe,
            "the Web socket's grid must hold the pipe, not the TUI ({tui_pipe}); got {owner:?}"
        );
        let _ = ws.close(None).await;
    });
    h.assert_screen_contains("Live send ended");
}
