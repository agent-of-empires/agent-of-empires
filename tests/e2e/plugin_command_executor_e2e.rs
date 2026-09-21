//! The native structured view runs plugin commands against a live daemon's
//! plugin worker (#2528): an `open-ui-link` chord opens the badge href (via
//! `AOE_OPEN_URL_TO`), and an action-less chord reaches the worker as
//! `plugin.command.invoke`.

use std::path::Path;
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_node, require_tmux, TuiTestHarness};

/// Pushes a per-session `detail-badge` with an href and writes a marker on
/// `plugin.command.invoke`.
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
    for args in [
        &["plugin", "install", src.to_str().unwrap(), "--yes"][..],
        &["plugin", "enable", "acme.gh"],
    ] {
        let out = h.run_cli(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let opened = h.home_path().join("opened-urls.txt");
    h.set_env("AOE_OPEN_URL_TO", opened.to_str().unwrap());

    let (_, session_id) = h.start_structured_session("plugin-exec");
    h.spawn(&["acp", "attach", &session_id]);
    h.wait_for("Message the agent");

    resend_until_file_contains(&h, "C-g", &opened, "https://example.com/pr/open");
    resend_until_file_contains(
        &h,
        "C-b",
        &h.home_path().join("plugin-invoke-marker"),
        "plugin.acme.gh.refresh",
    );
}
