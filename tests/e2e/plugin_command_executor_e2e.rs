//! Full-stack e2e: the native TUI structured view executes plugin commands
//! against a live daemon running a real plugin worker.
//!
//! A fake worker plugin (a small Node script) declares two commands and their
//! keybinds: `open_pr` (an `open-ui-link` client action) bound to `Ctrl+G`, and
//! an action-less `refresh` bound to `Ctrl+B`. The worker polls `sessions.list`
//! and pushes a per-session `detail-badge` carrying an href, and writes a marker
//! file when it receives a `plugin.command.invoke` notification.
//!
//! The test proves both surfaces of #2528 end to end:
//!   1. Pressing the `open_pr` chord resolves the badge's href from the plugin
//!      UI snapshot and opens it. The open goes through `tui::open_url`, which
//!      `AOE_OPEN_URL_TO` redirects to a file, so the resolved URL is asserted
//!      without spawning a real browser.
//!   2. Pressing the action-less `refresh` chord POSTs the invoke endpoint and
//!      the worker receives `plugin.command.invoke` (it writes the marker).
//!
//! Compiled only with the default `serve` feature (the structured view and the
//! plugin host don't exist otherwise). Run via:
//!
//! ```sh
//! cargo test --features e2e-tests --test e2e -- plugin_command_executor
//! ```
#![cfg(feature = "serve")]

use std::time::{Duration, Instant};

use serial_test::serial;

use crate::harness::{pick_free_port, require_node, require_tmux, wait_for_port, TuiTestHarness};

/// Minimal fake-ACP script: no turns, so the structured-view session attaches
/// and renders but the agent just idles. This test drives plugin commands, not
/// the agent.
const IDLE_ACP_SCRIPT: &str = r#"{ "turns": [] }"#;

/// The worker: newline-delimited JSON-RPC over stdio. It polls `sessions.list`
/// and pushes a per-session `detail-badge` with an href, and writes a marker
/// file when the host dispatches `plugin.command.invoke`.
const WORKER_JS: &str = r#"
const readline = require('readline');
const fs = require('fs');
const path = require('path');
let nextId = 0;
const send = (method, params) => {
  process.stdout.write(JSON.stringify({ jsonrpc: '2.0', id: ++nextId, method, params }) + '\n');
};
const marker = path.join(process.env.HOME || '.', 'plugin-invoke-marker');
const rl = readline.createInterface({ input: process.stdin });
rl.on('line', (line) => {
  let m;
  try { m = JSON.parse(line); } catch { return; }
  if (m.method === 'plugin.command.invoke') {
    fs.writeFileSync(marker, JSON.stringify(m.params || {}));
    return;
  }
  // A sessions.list response: push a PR badge for each session.
  if (m.result && Array.isArray(m.result.sessions)) {
    for (const s of m.result.sessions) {
      send('ui.state.set', {
        slot: 'detail-badge',
        id: 'pr',
        session_id: s.id,
        payload: { text: 'PR', href: 'https://example.com/pr/open' },
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
id = "refresh"
title = "Refresh"

[[keybinds]]
command = "open_pr"
key = "Ctrl+G"

[[keybinds]]
command = "refresh"
key = "Ctrl+B"

[[ui]]
slot = "detail-badge"
id = "pr"

[runtime]
kind = "command"
system = true
command = ["node", "{worker_rel}"]
"#
    )
}

fn parse_session_id(add_stdout: &str) -> String {
    add_stdout
        .lines()
        .find_map(|l| l.trim().strip_prefix("ID:"))
        .map(|rest| rest.trim().to_string())
        .unwrap_or_else(|| panic!("could not find session ID in `aoe add` output:\n{add_stdout}"))
}

/// Re-send `chord` on a cadence until `file` exists and contains `needle`, so
/// the test tolerates the plugin UI snapshot not having propagated to the TUI on
/// the first keypress. Panics with context on timeout.
fn resend_until_file_contains(
    h: &TuiTestHarness,
    chord: &str,
    file: &std::path::Path,
    needle: &str,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    loop {
        h.send_keys(chord);
        std::thread::sleep(Duration::from_millis(500));
        if let Ok(contents) = std::fs::read_to_string(file) {
            if contents.contains(needle) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "after resending {chord}, {} never contained {needle:?}. current: {:?}",
            file.display(),
            std::fs::read_to_string(file).ok(),
        );
    }
}

#[test]
#[serial]
fn tui_executes_plugin_commands_with_live_daemon() {
    require_tmux!();
    require_node!();

    let mut h = TuiTestHarness::new_in_tmp("plugin_command_executor");

    // A valid (idle) ACP agent so the structured-view session attaches.
    let script_path = h.home_path().join("idle-acp.json");
    std::fs::write(&script_path, IDLE_ACP_SCRIPT).expect("write fake-acp script");
    h.install_acp_shim(&script_path);

    // Install the fake worker plugin: manifest + worker.js in one dir.
    let src = h.home_path().join("src-plugin");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("worker.js"), WORKER_JS).unwrap();
    std::fs::write(src.join("aoe-plugin.toml"), manifest("worker.js")).unwrap();
    let installed = h.run_cli(&["plugin", "install", src.to_str().unwrap(), "--yes"]);
    assert!(
        installed.status.success(),
        "plugin install failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&installed.stdout),
        String::from_utf8_lossy(&installed.stderr),
    );
    // Ensure it is enabled so the daemon's plugin host spawns its worker
    // (idempotent when install already enabled it).
    let enabled = h.run_cli(&["plugin", "enable", "acme.gh"]);
    assert!(
        enabled.status.success(),
        "plugin enable failed: {}",
        String::from_utf8_lossy(&enabled.stderr)
    );

    // Redirect browser opens to a file so the resolved URL is assertable. Set
    // before the attach process is spawned; the TUI reads it in tui::open_url.
    let opened = h.home_path().join("opened-urls.txt");
    h.set_env("AOE_OPEN_URL_TO", &opened.display().to_string());

    h.stop_daemon_on_drop();

    // A structured-view session needs a git repo as its workspace.
    let project = h.project_path();
    for args in [
        vec!["init", "-q"],
        vec!["commit", "--allow-empty", "-q", "-m", "init"],
    ] {
        let out = std::process::Command::new("git")
            .args(&args)
            .current_dir(&project)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("run git");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let port = pick_free_port();
    let port_s = port.to_string();
    let start = h.run_cli(&["serve", "--daemon", "--port", &port_s, "--no-auth"]);
    assert!(
        start.status.success(),
        "aoe serve --daemon failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr),
    );
    assert!(
        wait_for_port(port, Duration::from_secs(10)),
        "daemon never bound port {port}"
    );

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "plugin-exec",
        "-c",
        "claude",
        "--structured-view",
    ]);
    assert!(
        add.status.success(),
        "aoe add --structured-view failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&add.stdout),
        String::from_utf8_lossy(&add.stderr),
    );
    let session_id = parse_session_id(&String::from_utf8_lossy(&add.stdout));

    // Attach the native TUI structured view over tmux (same HOME, so it finds
    // the local daemon).
    h.spawn(&["acp", "attach", &session_id]);
    h.wait_for("Message the agent");

    // 1. open-ui-link: the chord opens the badge's href (via the seam). Retry to
    //    absorb plugin-UI-snapshot propagation lag.
    resend_until_file_contains(
        &h,
        "C-g",
        &opened,
        "https://example.com/pr/open",
        Duration::from_secs(20),
    );

    // 2. action-less: the chord POSTs the invoke endpoint and the worker gets
    //    plugin.command.invoke (it writes the marker under HOME).
    let marker = h.home_path().join("plugin-invoke-marker");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        h.send_keys("C-b");
        std::thread::sleep(Duration::from_millis(500));
        if let Ok(contents) = std::fs::read_to_string(&marker) {
            assert!(
                contents.contains("plugin.acme.gh.refresh"),
                "invoke marker missing the command fqid: {contents}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "action-less chord never reached the worker (no marker at {})",
            marker.display(),
        );
    }
}
