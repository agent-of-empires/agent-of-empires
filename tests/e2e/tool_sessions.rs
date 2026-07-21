//! E2E coverage for the [tools.*] feature: picker dialog, command-palette
//! integration, the invalid-hotkey info dialog, and the full attach +
//! cleanup roundtrip against a real agent session.

use serial_test::serial;
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

#[test]
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
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
#[serial]
fn test_invalid_hotkey_surfaces_info_dialog() {
    require_tmux!();

    let mut h = TuiTestHarness::new("tool_invalid_hotkey");
    append_tools_config(
        &h,
        r#"
[tools.bad]
command = "echo hi"
hotkey = "Ctrl+x"
"#,
    );
    h.spawn_tui();

    // The startup info dialog should mention the broken entry.
    h.wait_for("Tool hotkey config errors");
    h.assert_screen_contains("bad");
    h.assert_screen_contains("Ctrl+x");
}

#[test]
#[serial]
fn test_tool_session_full_attach_and_cleanup_roundtrip() {
    require_tmux!();

    // Marker we'll grep for in the preview pane to prove the tool ran in
    // the right working directory and the preview cache captured it.
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

    // Create an agent session for the tool to attach to. The harness's
    // claude stub exits immediately, but the session row stays in the
    // list (Idle status). That's all we need to drive the tool flow.
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

    // Press the configured hotkey (Alt+t). tmux's send-keys grammar
    // names Alt-modified keys as `M-<key>`.
    h.send_keys("M-t");

    // Title flips to the new "Tool: <name>" prefix added in this PR.
    h.wait_for("Tool: echotool");

    // Pressing Enter triggers AttachToolSession, which (a) creates the
    // tool tmux session via `tmux new-session` running our command, then
    // (b) tries to switch-client / attach-session. Both attach paths
    // return errors when invoked from inside the harness's existing
    // tmux session ("sessions should be nested with care"), but that
    // error is swallowed and the tool tmux session itself is created.
    // After the failed attach, the TUI redraws and the preview cache
    // picks up the tool's stdout on its next 250ms refresh.
    h.send_keys("Enter");

    // Wait for the preview cache to pick up the tool's output. The cache
    // refreshes only when the TUI redraws (every 120ms when there's an
    // animated spinner, every 5s on disk refresh, or on any key event).
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

    // Remove the agent session. `perform_deletion` invokes
    // `kill_all_tool_sessions_for_id`, which runs `tmux list-sessions` /
    // `kill-session` against the tmux socket aoe resolves. aoe now routes
    // every tmux call through an explicit `-S <socket>` (#2608), so point
    // `AOE_TMUX_SOCKET` at the harness's per-test socket to exercise the
    // sweep there instead of aoe's own app-dir socket.
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
