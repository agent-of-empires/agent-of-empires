//! E2E coverage for the [tools.*] feature: picker dialog, command-palette
//! integration, and the full attach + cleanup roundtrip against a real agent
//! session.

use serial_test::parallel;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

/// Append a `[tools.*]` block to the harness's pre-seeded config.toml.
fn append_tools_config(h: &TuiTestHarness, body: &str) {
    let path = app_dir_in(h.home_path()).join("config.toml");
    let existing = fs::read_to_string(&path).expect("read pre-seeded config.toml");
    fs::write(&path, format!("{existing}\n{body}\n")).expect("write tools config");
}

/// List tmux session names on a specific socket. Tool sessions spawned by
/// `aoe` while running inside the harness's tmux land on the harness's
/// per-test socket (because `TMUX` env points there), so callers pass
/// the harness's socket here to verify creation and sweep.
fn list_tmux_sessions_on(socket: &std::path::Path) -> Vec<String> {
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

/// Defensive teardown for any tool tmux session that survived a test
/// (e.g., when the test panics before the cleanup assertion).
fn kill_lingering_tool_sessions_on(socket: &std::path::Path, prefix_marker: &str) {
    for name in list_tmux_sessions_on(socket) {
        if name.starts_with("aoe_dev_tool_") && name.contains(prefix_marker) {
            let _ = Command::new("tmux")
                .arg("-S")
                .arg(socket)
                .args(["kill-session", "-t", &name])
                .output();
        }
    }
}

fn shell_quote_path(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn wait_for_file_contents(path: &Path, timeout: Duration) -> String {
    let start = Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(path) {
            if !contents.trim().is_empty() {
                return contents;
            }
        }
        if start.elapsed() >= timeout {
            panic!("timed out waiting for {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until the daemon has published `generation` with no live lifecycle
/// reservation for `id`.
///
/// This test process writes `sessions.json` directly, so the row only reaches
/// the daemon's canonical in-memory state through its debounced disk watch.
/// A mutation issued before that convergence is rejected as superseded, so
/// callers that write the file out of process must wait for it first.
async fn wait_for_lifecycle_published(
    http: &reqwest::Client,
    id: &str,
    generation: u64,
    timeout: Duration,
) {
    let start = Instant::now();
    loop {
        let snapshot: agent_of_empires::daemon::RuntimeSnapshot = http
            .get("http://localhost/api/runtime/snapshot")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let published = snapshot
            .contents
            .sessions
            .iter()
            .find(|value| value.id == id)
            .expect("session missing from the published snapshot");
        if published.lifecycle_generation == generation && published.lifecycle_reservation.is_none()
        {
            return;
        }
        assert!(
            start.elapsed() < timeout,
            "generation {generation} without a reservation was not published"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[test]
#[parallel]
fn test_tool_picker_lists_configured_tools() {
    require_tmux!();

    let mut h = TuiTestHarness::new("tool_picker_list");
    append_tools_config(
        &h,
        r#"
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"

[tools.yazi]
command = "yazi"
"#,
    );
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("\\;");
    h.wait_for("Tool Sessions");
    h.assert_screen_contains("lazygit");
    h.assert_screen_contains("yazi");
    // Footer hint added in this PR.
    h.assert_screen_contains("Enter");
    h.assert_screen_contains("Esc");

    // Re-press ; to close (toggle behavior added in this PR).
    h.send_keys("\\;");
    h.wait_for_absent("Tool Sessions", Duration::from_secs(5));
}

#[test]
#[parallel]
fn test_tool_picker_does_not_open_with_no_tools_configured() {
    require_tmux!();

    let mut h = TuiTestHarness::new("tool_picker_empty");
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("\\;");
    // With zero [tools.*] entries the picker is suppressed; the screen
    // should still be on the home view.
    std::thread::sleep(Duration::from_millis(200));
    h.assert_screen_not_contains("Tool Sessions");
}

#[test]
#[parallel]
fn test_command_palette_includes_tool_entries() {
    require_tmux!();

    let mut h = TuiTestHarness::new("tool_palette_entry");
    append_tools_config(
        &h,
        r#"
[tools.lazygit]
command = "lazygit"
hotkey = "Alt+g"
"#,
    );
    h.spawn_tui();

    h.wait_for(" aoe ");
    h.send_keys("C-k");
    h.wait_for("Commands");
    h.type_text("lazyg");
    std::thread::sleep(Duration::from_millis(150));
    h.assert_screen_contains("Open tool: lazygit");
}

#[test]
#[parallel]
fn test_background_tool_runs_without_tmux_session_or_preview_switch() {
    require_tmux!();

    let mut h = TuiTestHarness::new("tool_background_run");
    let output_path = h.home_path().join("background-tool-pwd.txt");
    append_tools_config(
        &h,
        &format!(
            r#"
[tools.bgpwd]
command = "pwd > {}"
hotkey = "Alt+b"
background = true
"#,
            shell_quote_path(&output_path),
        ),
    );

    let project = h.project_path();
    let expected_project = fs::canonicalize(&project).expect("canonicalize project path");
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "BackgroundToolSession",
    ]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let sessions_path = app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    let sessions_str = fs::read_to_string(&sessions_path).expect("read sessions.json");
    let sessions: serde_json::Value =
        serde_json::from_str(&sessions_str).expect("parse sessions.json");
    let session_id = sessions[0]["id"]
        .as_str()
        .expect("session id present in sessions.json")
        .to_string();
    let id_suffix = &session_id[..session_id.len().min(8)];
    let harness_sock = h.home_path().join("tmux.sock");
    kill_lingering_tool_sessions_on(&harness_sock, id_suffix);

    h.spawn_tui();
    h.wait_for("BackgroundToolSession");

    h.send_keys("C-k");
    h.wait_for("Commands");
    h.type_text("bgpwd");
    h.wait_for("Run: bgpwd");
    h.send_keys("Enter");
    let contents = wait_for_file_contents(&output_path, Duration::from_secs(5));
    assert_eq!(contents.trim(), expected_project.to_string_lossy());
    h.assert_screen_not_contains("Tool: bgpwd");

    fs::remove_file(&output_path).expect("remove palette output");
    h.send_keys("M-b");
    let contents = wait_for_file_contents(&output_path, Duration::from_secs(5));
    assert_eq!(contents.trim(), expected_project.to_string_lossy());
    h.assert_screen_not_contains("Tool: bgpwd");

    fs::remove_file(&output_path).expect("remove hotkey output");
    h.send_keys("\\;");
    h.wait_for("Tool Sessions");
    h.assert_screen_contains("[bg]");
    h.send_keys("Enter");
    let contents = wait_for_file_contents(&output_path, Duration::from_secs(5));
    assert_eq!(contents.trim(), expected_project.to_string_lossy());
    h.assert_screen_not_contains("Tool: bgpwd");

    let sessions = list_tmux_sessions_on(&harness_sock);
    assert!(
        !sessions
            .iter()
            .any(|s| s.starts_with("aoe_dev_tool_") && s.contains(id_suffix)),
        "background tool should not create a tmux tool session. sessions seen: {:?}",
        sessions
    );
}

#[test]
#[parallel]
fn test_tool_session_full_attach_and_cleanup_roundtrip() {
    require_tmux!();

    const MARKER: &str = "TOOL_OUTPUT_ROUNDTRIP_MARKER";

    let mut h = TuiTestHarness::new("tool_roundtrip");
    append_tools_config(
        &h,
        &format!(
            r#"
[tools.echotool]
command = "while true; do echo {MARKER}; sleep 0.5; done"
hotkey = "Alt+t"
"#
        ),
    );

    let project = h.project_path();
    let add = h.run_cli(&["add", project.to_str().unwrap(), "-t", "RoundtripSession"]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    // Find the session ID for the later remove + cleanup-sweep assertion.
    let sessions_path = app_dir_in(h.home_path()).join("profiles/default/sessions.json");
    let sessions_str = fs::read_to_string(&sessions_path).expect("read sessions.json");
    let sessions: serde_json::Value =
        serde_json::from_str(&sessions_str).expect("parse sessions.json");
    let session_id = sessions[0]["id"]
        .as_str()
        .expect("session id present in sessions.json")
        .to_string();
    let id_suffix = &session_id[..session_id.len().min(8)];

    // The harness's tmux server uses a per-test socket. Tool tmux sessions
    // spawned by aoe (running inside the harness's tmux) inherit `TMUX` and
    // land on the *same* socket, not the system default. We inspect and
    // clean up on that socket.
    let harness_sock = h.home_path().join("tmux.sock");

    // Defensive: kill any stale tool sessions from a previous aborted run.
    kill_lingering_tool_sessions_on(&harness_sock, id_suffix);

    h.spawn_tui();
    h.wait_for("RoundtripSession");
    h.wait_for("Runtime ready");

    // Press the configured hotkey (Alt+t). tmux's send-keys grammar
    // names Alt-modified keys as `M-<key>`.
    h.send_keys("M-t");

    h.wait_for("Tool: echotool");

    // Pressing Enter triggers AttachToolSession, which (a) creates the
    // tool tmux session via `tmux new-session` running our command, then
    // (b) tries to switch-client / attach-session. Both attach paths
    // return errors when invoked from inside the harness's existing
    // tmux session ("sessions should be nested with care"), but that
    // error is swallowed and the tool tmux session itself is created.
    // Observe the recreated outer EventStream before sending its render fence.
    let resume = h.terminal_resume_sequence();
    h.send_keys("Enter");
    h.wait_for_terminal_resume(resume);

    h.wait_for(MARKER);

    // Esc returns to the structured view.
    h.send_keys("Escape");
    h.wait_for_absent("Tool: echotool", Duration::from_secs(5));

    // The tool tmux session should still exist on the harness socket
    // until we remove the parent agent session.
    let pre_remove = list_tmux_sessions_on(&harness_sock);
    assert!(
        pre_remove
            .iter()
            .any(|s| s.starts_with("aoe_dev_tool_") && s.contains(id_suffix)),
        "expected a live aoe_dev_tool_* session matching id suffix {} before removal. \
         sessions seen: {:?}",
        id_suffix,
        pre_remove
    );

    // Quit the TUI so the removal CLI can write sessions.json without
    // racing the TUI's poller. `q` opens the quit confirmation (#1569),
    // so confirm with `y` to actually exit.
    h.send_keys("q");
    h.wait_for("Quit Agent of Empires");
    h.send_keys("y");
    h.wait_for_exit(Duration::from_secs(5));

    // Run removal against the harness socket containing the tool panes.
    let aoe_binary = env!("CARGO_BIN_EXE_aoe");
    let remove = Command::new(aoe_binary)
        .args(["remove", &session_id, "--force"])
        .env("HOME", h.home_path())
        .env("XDG_CONFIG_HOME", h.home_path().join(".config"))
        .env("AOE_TMUX_SOCKET", &harness_sock)
        .env_remove("AGENT_OF_EMPIRES_DEBUG")
        .env_remove("AOE_LOG_LEVEL")
        .output()
        .expect("run aoe remove");
    assert!(
        remove.status.success(),
        "aoe remove failed: {}",
        String::from_utf8_lossy(&remove.stderr)
    );

    // Verify the sweep landed.
    let post_remove = list_tmux_sessions_on(&harness_sock);
    let leaked: Vec<_> = post_remove
        .iter()
        .filter(|s| s.starts_with("aoe_dev_tool_") && s.contains(id_suffix))
        .collect();
    assert!(
        leaked.is_empty(),
        "tool sessions leaked after `aoe remove`: {:?}",
        leaked
    );

    // Belt-and-suspenders: clean up anything else we created, in case
    // the assertion above passes but other state hangs around.
    kill_lingering_tool_sessions_on(&harness_sock, id_suffix);
}

#[cfg(unix)]
#[tokio::test]
#[parallel]
async fn native_auxiliary_ensure_uses_fresh_context_and_refuses_purge() {
    use agent_of_empires::{
        daemon::DaemonClient,
        session::{Instance, LifecycleOperation, Status},
    };
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("native_tool_ensure");
    h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
    h.stop_daemon_on_drop();
    let app = app_dir_in(h.home_path());
    let first_output = h.home_path().join("first-tool-cwd");
    let second_output = h.home_path().join("second-tool-cwd");
    let command = |output: &Path| format!("pwd > {}; exec sleep 120", shell_quote_path(output));
    let first_command = serde_json::to_string(&command(&first_output)).unwrap();
    append_tools_config(&h, &format!("[tools.\"..\"]\ncommand = {first_command}"));
    let mut row = Instance::new("native tool", h.project_path().to_str().unwrap());
    row.status = Status::Stopped;
    row.terminal_info = Some(agent_of_empires::session::TerminalInfo { created: true });
    let rows_path = app.join("profiles/default/sessions.json");
    let persist =
        |row: &Instance| fs::write(&rows_path, serde_json::to_vec(&[row]).unwrap()).unwrap();
    persist(&row);
    let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
    assert!(
        started.status.success(),
        "{}",
        String::from_utf8_lossy(&started.stderr)
    );
    let sdk = DaemonClient::new_unix(app.join("daemon/api.sock")).unwrap();
    let epoch = sdk.runtime_info().await.unwrap().epoch;
    let http = reqwest::Client::builder()
        .unix_socket(app.join("daemon/api.sock"))
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let inspect_pane = |name: &str| {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(h.home_path().join("tmux.sock"))
            .args([
                "display-message",
                "-p",
                "-t",
                &format!("={name}:^.0"),
                "#{pane_pid}\t#{pane_width}\t#{pane_height}\t#{pane_current_path}",
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    let host_url = format!("http://localhost/api/sessions/{}/terminal", row.id);
    let ensure_host = || {
        http.post(&host_url)
            .header(agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, &epoch)
            .json(&serde_json::json!({"size": {"cols": 71, "rows": 29}}))
            .send()
    };
    let host = ensure_host().await.unwrap();
    assert_eq!(
        host.status(),
        reqwest::StatusCode::CREATED,
        "a persisted cache flag is not a live terminal"
    );
    assert!(host
        .headers()
        .contains_key(agent_of_empires::daemon::RUNTIME_REVISION_HEADER));
    let host: serde_json::Value = host.json().await.unwrap();
    let host_name = host["tmux_session"].as_str().unwrap();
    let original_host = inspect_pane(host_name);
    assert_eq!(original_host.split('\t').nth(1), Some("71"));
    assert_eq!(original_host.split('\t').nth(2), Some("29"));
    row.terminal_info = None;
    persist(&row);
    let ensured_host = sdk
        .ensure_terminal(
            &row.id,
            0,
            &agent_of_empires::daemon::StartSessionBody::default(),
            &epoch,
        )
        .await
        .unwrap();
    assert!(matches!(
        ensured_host.outcome.status,
        agent_of_empires::daemon::TerminalTargetStatus::Exists
    ));
    assert_eq!(ensured_host.outcome.tmux_session, host_name);
    assert_eq!(inspect_pane(host_name), original_host);
    let durable: Vec<Instance> = serde_json::from_slice(&fs::read(&rows_path).unwrap()).unwrap();
    assert!(
        durable[0].has_terminal(),
        "terminal creation was not committed"
    );
    row.terminal_info = durable[0].terminal_info.clone();
    let url = format!("http://localhost/api/sessions/{}/tools/ensure", row.id);
    let ensure = || {
        http.post(&url)
            .header(agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, &epoch)
            .json(&serde_json::json!({"tool_name": "..", "size": {"cols": 91, "rows": 27}}))
            .send()
    };
    let injected = http
        .post(&url)
        .header(agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, &epoch)
        .json(&serde_json::json!({"tool_name": "..", "command": "exit 0", "cwd": "/"}))
        .send()
        .await
        .unwrap();
    assert_eq!(injected.status(), reqwest::StatusCode::UNPROCESSABLE_ENTITY);
    let response = ensure().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.headers()[agent_of_empires::daemon::RUNTIME_EPOCH_HEADER],
        epoch
    );
    assert!(response
        .headers()
        .contains_key(agent_of_empires::daemon::RUNTIME_REVISION_HEADER));
    let target: serde_json::Value = response.json().await.unwrap();
    let name = target["tmux_session"].as_str().unwrap();
    let pane = || inspect_pane(name);
    assert_eq!(
        wait_for_file_contents(&first_output, Duration::from_secs(5)).trim(),
        row.project_path
    );
    let original_pane = pane();
    assert_eq!(
        original_pane.split('\t').nth(1),
        Some("91"),
        "target {name}: {original_pane:?}"
    );
    assert_eq!(original_pane.split('\t').nth(2), Some("27"));
    let generation = row
        .try_acquire_lifecycle_reservation(
            LifecycleOperation::Purge,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )
        .unwrap();
    persist(&row);
    assert_eq!(
        ensure().await.unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    assert_eq!(
        ensure_host().await.unwrap().status(),
        reqwest::StatusCode::CONFLICT
    );
    let refused_stop = http
        .post(format!(
            "http://localhost/api/sessions/{}/auxiliary/stop",
            row.id
        ))
        .header(agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, &epoch)
        .json(&serde_json::json!({"kind": "host", "index": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused_stop.status(), reqwest::StatusCode::CONFLICT);
    let refused_pair = http
        .delete(format!(
            "http://localhost/api/sessions/{}/terminal?index=1",
            row.id
        ))
        .header(agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, &epoch)
        .send()
        .await
        .unwrap();
    assert_eq!(refused_pair.status(), reqwest::StatusCode::CONFLICT);
    let stream = tokio::net::UnixStream::connect(app.join("daemon/api.sock"))
        .await
        .unwrap();
    let websocket = tokio_tungstenite::client_async(
        format!("ws://localhost/sessions/{}/terminal/live-ws", row.id),
        stream,
    )
    .await;
    let Err(tokio_tungstenite::tungstenite::Error::Http(response)) = websocket else {
        panic!("a reserved purge admitted a host-terminal WebSocket");
    };
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(inspect_pane(host_name), original_host);
    assert_eq!(pane(), original_pane);
    row.release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
    persist(&row);
    wait_for_lifecycle_published(&http, &row.id, generation, Duration::from_secs(10)).await;
    let config_path = app.join("config.toml");
    let config = fs::read_to_string(&config_path).unwrap();
    fs::write(
        &config_path,
        config.replace(
            &first_command,
            &serde_json::to_string(&command(&second_output)).unwrap(),
        ),
    )
    .unwrap();
    let ensured = sdk
        .ensure_tool(
            &row.id,
            &agent_of_empires::daemon::EnsureToolBody {
                tool_name: "..".into(),
                size: None,
            },
            &epoch,
        )
        .await
        .unwrap();
    assert_eq!(ensured.outcome.tmux_session, name);
    assert_eq!(pane(), original_pane);
    assert!(
        !second_output.exists(),
        "ensure restarted a live tool after configuration changed"
    );
    let renamed = agent_of_empires::tmux::ToolSession::generate_name(&row.id, "Renamed", "..");
    let rename = |from: &str, to: &str| {
        let output = Command::new("tmux")
            .arg("-S")
            .arg(h.home_path().join("tmux.sock"))
            .args(["rename-session", "-t", &format!("={from}:"), to])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    rename(name, &renamed);
    let renamed_receipt = sdk
        .ensure_tool(
            &row.id,
            &agent_of_empires::daemon::EnsureToolBody {
                tool_name: "..".into(),
                size: None,
            },
            &epoch,
        )
        .await
        .unwrap();
    assert_eq!(renamed_receipt.outcome.tmux_session, renamed);
    let snapshot: agent_of_empires::daemon::RuntimeSnapshot = http
        .get("http://localhost/api/runtime/snapshot")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(snapshot.cursor.epoch, epoch);
    assert!(snapshot.cursor.revision >= renamed_receipt.cursor.revision);
    let published = snapshot
        .contents
        .sessions
        .iter()
        .find(|item| item.id == row.id)
        .unwrap();
    for (target, expected_name) in [
        (
            agent_of_empires::session::AuxiliaryTarget::Host { index: 0 },
            host_name,
        ),
        (
            agent_of_empires::session::AuxiliaryTarget::Tool {
                tool_name: "..".into(),
            },
            renamed.as_str(),
        ),
    ] {
        let observed = published
            .auxiliary
            .iter()
            .find(|item| item.target == target)
            .unwrap();
        assert_eq!(
            observed.pane.state,
            agent_of_empires::session::PanePresence::Alive
        );
        assert_eq!(observed.pane.tmux_session.as_deref(), Some(expected_name));
    }
    rename(&renamed, name);
    assert_eq!(pane(), original_pane);
    let killed = Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args(["kill-session", "-t", &format!("={name}")])
        .output()
        .unwrap();
    assert!(killed.status.success());
    let fresh = h.project_path().join("fresh-tool-cwd");
    fs::create_dir(&fresh).unwrap();
    row.project_path = fresh.to_string_lossy().into_owned();
    persist(&row);
    let recreated = sdk
        .ensure_tool(
            &row.id,
            &agent_of_empires::daemon::EnsureToolBody {
                tool_name: "..".into(),
                size: None,
            },
            &epoch,
        )
        .await
        .unwrap();
    assert_eq!(recreated.outcome.tmux_session, name);
    assert_eq!(
        wait_for_file_contents(&second_output, Duration::from_secs(5)).trim(),
        row.project_path
    );
    assert_eq!(
        pane().trim().rsplit('\t').next(),
        Some(row.project_path.as_str())
    );
    let sent = Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args([
            "send-keys",
            "-t",
            &format!("={host_name}:^.0"),
            "exit",
            "Enter",
        ])
        .output()
        .unwrap();
    assert!(sent.status.success());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let dead = Command::new("tmux")
            .arg("-S")
            .arg(h.home_path().join("tmux.sock"))
            .args([
                "display-message",
                "-p",
                "-t",
                &format!("={host_name}:^.0"),
                "#{pane_dead}",
            ])
            .output()
            .unwrap();
        assert!(dead.status.success());
        if dead.stdout == b"1\n" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "host shell did not exit"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    row.terminal_info = None;
    persist(&row);
    let snapshot = || async {
        http.get("http://localhost/api/runtime/snapshot")
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<agent_of_empires::daemon::RuntimeSnapshot>()
            .await
            .unwrap()
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while snapshot()
        .await
        .contents
        .sessions
        .iter()
        .find(|value| value.id == row.id)
        .unwrap()
        .has_terminal
    {
        assert!(
            std::time::Instant::now() < deadline,
            "cleared terminal flag was not reflected"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let stream = tokio::net::UnixStream::connect(app.join("daemon/api.sock"))
        .await
        .unwrap();
    let (mut websocket, response) = tokio_tungstenite::client_async(
        format!("ws://localhost/sessions/{}/terminal/live-ws", row.id),
        stream,
    )
    .await
    .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::SWITCHING_PROTOCOLS);
    assert!(
        snapshot()
            .await
            .contents
            .sessions
            .iter()
            .find(|value| value.id == row.id)
            .unwrap()
            .has_terminal,
        "WebSocket recovery granted access before publishing the terminal commit"
    );
    let revived_host = inspect_pane(host_name);
    assert_ne!(
        revived_host.split('\t').next(),
        original_host.split('\t').next()
    );
    assert_eq!(
        revived_host.trim().rsplit('\t').next(),
        Some(row.project_path.as_str())
    );
    websocket.close(None).await.unwrap();
    let mut stored: Vec<Instance> = serde_json::from_slice(&fs::read(&rows_path).unwrap()).unwrap();
    let container_row = &mut stored[0];
    container_row.sandbox_info = Some(agent_of_empires::session::SandboxInfo {
        enabled: true,
        container_id: None,
        image: "unused-existing-pane".into(),
        container_name: agent_of_empires::containers::DockerContainer::generate_name(
            &container_row.id,
        ),
        extra_env: None,
        custom_instruction: None,
        container_workdir: None,
        before_start_env: Vec::new(),
    });
    persist(container_row);
    let container_name = container_row
        .container_terminal_tmux_session_indexed(0)
        .unwrap()
        .name()
        .to_owned();
    // Existing-pane admission must not require a container backend.
    let created = Command::new("tmux")
        .arg("-S")
        .arg(h.home_path().join("tmux.sock"))
        .args(["new-session", "-d", "-s", &container_name, "exec sleep 120"])
        .output()
        .unwrap();
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    let original_container = inspect_pane(&container_name);
    let target = sdk
        .ensure_container_terminal(
            &container_row.id,
            0,
            &agent_of_empires::daemon::StartSessionBody::default(),
            &epoch,
        )
        .await
        .unwrap();
    assert_eq!(target.outcome.tmux_session, container_name);
    assert!(matches!(
        target.outcome.status,
        agent_of_empires::daemon::TerminalTargetStatus::Exists
    ));
    let generation = container_row
        .try_acquire_lifecycle_reservation(
            LifecycleOperation::Purge,
            Instance::LIFECYCLE_RESERVATION_TTL,
            chrono::Utc::now(),
        )
        .unwrap();
    persist(container_row);
    let refused = http
        .post(format!(
            "http://localhost/api/sessions/{}/container-terminal",
            container_row.id
        ))
        .header(agent_of_empires::daemon::RUNTIME_EPOCH_HEADER, &epoch)
        .json(&agent_of_empires::daemon::StartSessionBody::default())
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::CONFLICT);
    let stream = tokio::net::UnixStream::connect(app.join("daemon/api.sock"))
        .await
        .unwrap();
    let websocket = tokio_tungstenite::client_async(
        format!(
            "ws://localhost/sessions/{}/container-terminal/live-ws",
            container_row.id
        ),
        stream,
    )
    .await;
    let Err(tokio_tungstenite::tungstenite::Error::Http(response)) = websocket else {
        panic!("a reserved purge admitted a container-terminal WebSocket");
    };
    assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
    assert_eq!(inspect_pane(&container_name), original_container);
    container_row.release_lifecycle_reservation_if_owned(LifecycleOperation::Purge, generation);
    container_row.sandbox_info = None;
    persist(container_row);
}
