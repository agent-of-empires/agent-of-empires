//! The native structured view runs plugin commands against a live daemon's
//! plugin worker (#2528): an `open-ui-link` chord opens the badge href (via
//! `AOE_OPEN_URL_TO`), and an action-less chord reaches the worker as
//! `plugin.command.invoke`. A plugin-supplied relative href (#4089) resolves
//! against the daemon's own base URL for both an `open-ui-link` command and a
//! `ui.open_url` notification, since the TUI has no browser origin of its own.

use std::path::Path;
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// Pushes an absolute- and a relative-href `detail-badge` per session, writes
/// a marker on `plugin.command.invoke`, and fires a relative-href
/// notification when the `refresh` command specifically is invoked.
const WORKER_JS: &str = r#"
const readline = require('readline');
const fs = require('fs');
const path = require('path');
let nextId = 0;
const send = (method, params) => {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: ++nextId, method, params }) + '\n');
};
const marker = path.join(process.env.HOME || '.', 'plugin-invoke-marker');
let lastSessionId = null;
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let m;
  try { m = JSON.parse(line); } catch { return; }
  if (m.method === 'plugin.command.invoke') {
    fs.writeFileSync(marker, JSON.stringify(m.params || {}));
    // Only on an explicit, TUI-driven action (never automatically off the
    // worker's own sessions.list poll), so this can never race the daemon's
    // baseline-the-first-snapshot-without-toasting logic (state.rs:284-299):
    // the test only presses this after already observing the TUI ingest a
    // snapshot via the badges below.
    if (m.params && m.params.command === 'plugin.acme.gh.refresh' && lastSessionId) {
      send('ui.open_url', { url: '/session/' + lastSessionId + '?notify=1', title: 'Relative notify' });
    }
    return;
  }
  // A sessions.list response: push PR badges for each session.
  if (m.result && Array.isArray(m.result.sessions)) {
    for (const s of m.result.sessions) {
      lastSessionId = s.id;
      send('ui.state.set', {
        slot: 'detail-badge',
        id: 'pr',
        session_id: s.id,
        payload: { text: 'PR', href: 'https://example.com/pr/open' },
      });
      send('ui.state.set', {
        slot: 'detail-badge',
        id: 'pr-relative',
        session_id: s.id,
        payload: { text: 'PR (relative)', href: '/session/' + s.id },
      });
    }
  }
});
setInterval(() => send('sessions.list', {}), 300);
"#;

fn manifest(worker_rel: &str) -> String {
    format!(
        r#"
id = "acme.gh"
name = "GH Test"
version = "0.1.0"
api_version = 8
capabilities = ["runtime.worker", "session.read", "browser_open"]

[[commands]]
id = "open_pr"
title = "Open PR"
[commands.action]
kind = "open-ui-link"
slot = "detail-badge"
id = "pr"

[[commands]]
id = "open_pr_relative"
title = "Open PR (relative)"
[commands.action]
kind = "open-ui-link"
slot = "detail-badge"
id = "pr-relative"

[[commands]]
id = "refresh"
title = "Refresh"

[[keybinds]]
command = "open_pr"
key = "Ctrl+G"

[[keybinds]]
command = "open_pr_relative"
key = "Ctrl+Y"

[[keybinds]]
command = "refresh"
key = "Ctrl+B"

[[ui]]
slot = "detail-badge"
id = "pr"

[[ui]]
slot = "detail-badge"
id = "pr-relative"

[runtime]
kind = "command"
system = true
command = ["node", "{worker_rel}"]
"#
    )
}

const ROW_RESOURCES_WORKER_JS: &str = r#"
const readline = require('readline');
let nextId = 0;
const send = (method, params) => {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: ++nextId, method, params }) + '\n');
};
const resources = {
  github: { id: 'pr', text: 'Open', tone: 'success' },
  linear: { id: 'issue', text: 'WIP', tone: 'info' },
};
const resource = resources[process.argv[2]];
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let message;
  try { message = JSON.parse(line); } catch { return; }
  if (message.result && Array.isArray(message.result.sessions)) {
    for (const session of message.result.sessions) {
      send('ui.state.set', {
        slot: 'row-badge',
        id: resource.id,
        session_id: session.id,
        payload: { text: resource.text, tone: resource.tone },
      });
    }
  }
});
setInterval(() => send('sessions.list', {}), 300);
"#;

fn row_status_manifest(
    worker_rel: &str,
    plugin_id: &str,
    name: &str,
    resource_id: &str,
    provider: &str,
) -> String {
    format!(
        r#"
id = "{plugin_id}"
name = "{name}"
version = "0.1.0"
api_version = 8
capabilities = ["runtime.worker", "session.read"]

[[ui]]
slot = "row-badge"
id = "{resource_id}"

[runtime]
kind = "command"
system = true
command = ["node", "{worker_rel}", "{provider}"]
"#
    )
}

/// Resend `chord` until `file` contains `needle`; the plugin UI snapshot may
/// not have reached the TUI on the first press.
fn resend_until_file_contains(h: &TuiTestHarness, chord: &str, file: &Path, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.send_keys(chord);
        std::thread::sleep(Duration::from_millis(500));
        if std::fs::read_to_string(file).is_ok_and(|c| c.contains(needle)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "after resending {chord}, {} never contained {needle:?}. current: {:?}",
            file.display(),
            std::fs::read_to_string(file).ok(),
        );
    }
}

/// Poll `file` for `needle`, for a source (a notification) that isn't
/// triggered by a resendable keypress.
fn wait_until_file_contains(file: &Path, needle: &str, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        if std::fs::read_to_string(file).is_ok_and(|c| c.contains(needle)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{} never contained {needle:?}. current: {:?}",
            file.display(),
            std::fs::read_to_string(file).ok(),
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
#[parallel]
fn tui_executes_plugin_commands_with_live_daemon() {
    require_tmux!();
    require_node!();
    let mut h = TuiTestHarness::new_acp("plugin_command_executor", r#"{ "turns": [] }"#);

    let src = h.home_path().join("src-plugin");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("worker.js"), WORKER_JS).unwrap();
    std::fs::write(src.join("aoe-plugin.toml"), manifest("worker.js")).unwrap();
    let installed = h.run_cli(&["plugin", "install", src.to_str().unwrap(), "--yes"]);
    assert!(
        installed.status.success(),
        "plugin install failed: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    // `plugin enable` goes through the Daemon, which must already run and see
    // the env below.
    let opened = h.home_path().join("opened-urls.txt");
    h.set_env("AOE_OPEN_URL_TO", opened.to_str().unwrap());
    let (port, session_id) = h.start_structured_session("plugin-exec");
    let enabled = h.run_cli(&["plugin", "enable", "acme.gh"]);
    assert!(
        enabled.status.success(),
        "plugin enable failed: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );
    h.spawn(&["acp", "attach", &session_id]);
    h.wait_for("Message the agent");

    // Establishes that the TUI has ingested at least one plugin UI snapshot
    // (its `plugin_notify` watermark is no longer at the unset baseline)
    // before anything below relies on a notification actually surfacing.
    resend_until_file_contains(&h, "C-g", &opened, "https://example.com/pr/open");

    // A relative href has no meaning to a browser launcher on its own (#4089):
    // both the command and the notification path must resolve it against the
    // daemon's own base URL before it reaches `open_url`.
    resend_until_file_contains(
        &h,
        "C-y",
        &opened,
        &format!("http://127.0.0.1:{port}/session/{session_id}\n"),
    );

    // `refresh` (C-b) both writes the invoke marker and, per WORKER_JS, fires
    // the relative-href notification — only now, strictly after the C-g
    // assertion above proved the TUI already ingested a snapshot.
    resend_until_file_contains(
        &h,
        "C-b",
        &h.home_path().join("plugin-invoke-marker"),
        "plugin.acme.gh.refresh",
    );
    wait_until_file_contains(
        &opened,
        &format!("http://127.0.0.1:{port}/session/{session_id}?notify=1"),
        Duration::from_secs(20),
    );
}
#[test]
#[parallel]
fn home_shows_plugin_row_statuses_from_enabled_plugins() {
    require_tmux!();
    require_node!();

    for (scenario, include_github, include_linear) in [
        ("both", true, true),
        ("github", true, false),
        ("linear", false, true),
    ] {
        run_plugin_row_status_scenario(scenario, include_github, include_linear);
    }
}

fn run_plugin_row_status_scenario(scenario: &str, include_github: bool, include_linear: bool) {
    let test_name = format!("plugin_row_statuses_{scenario}");
    let mut h = TuiTestHarness::new_in_tmp(&test_name);
    for (enabled, provider, plugin_id, name, resource_id) in [
        (include_github, "github", "test.github", "GitHub Test", "pr"),
        (
            include_linear,
            "linear",
            "test.linear",
            "Linear Test",
            "issue",
        ),
    ] {
        if !enabled {
            continue;
        }
        let src = h.home_path().join(format!("src-{provider}"));
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("worker.js"), ROW_RESOURCES_WORKER_JS).unwrap();
        std::fs::write(
            src.join("aoe-plugin.toml"),
            row_status_manifest("worker.js", plugin_id, name, resource_id, provider),
        )
        .unwrap();
        let installed = h.run_cli(&["plugin", "install", src.to_str().unwrap(), "--yes"]);
        assert!(
            installed.status.success(),
            "{provider} plugin install failed: {}",
            String::from_utf8_lossy(&installed.stderr)
        );
    }

    let script_path = h.home_path().join("idle-acp.json");
    std::fs::write(&script_path, r#"{ "turns": [] }"#).expect("write fake ACP script");
    h.install_acp_shim(&script_path);
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let output = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?} failed");
    }
    h.stop_daemon_on_drop();
    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "daemon start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(wait_for_port(port, Duration::from_secs(10)));
    h.tmux_new_detached("aoe_test_plugin_env", "sleep 60");
    h.spawn_tui_sized(160, 40);
    h.wait_for_ready();
    h.send_keys(">>>>>>");
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "Plugin Resources",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "session creation failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    if include_github {
        h.wait_for_timeout("Open", Duration::from_secs(30));
    }
    if include_linear {
        h.wait_for_timeout("WIP", Duration::from_secs(30));
    }
    let home_screen = h.capture_screen();
    let status_row = home_screen
        .lines()
        .find(|line| line.contains("Plugin Resources"))
        .unwrap_or_else(|| panic!("session row missing from screen: {home_screen}"));
    assert_eq!(
        status_row.contains("Open"),
        include_github,
        "GitHub row badge presence mismatch for {scenario}: {status_row}"
    );
    assert_eq!(
        status_row.contains("WIP"),
        include_linear,
        "Linear row badge presence mismatch for {scenario}: {status_row}"
    );
}
