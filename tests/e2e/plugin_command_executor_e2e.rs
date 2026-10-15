//! The native structured view runs plugin commands against a live daemon's
//! plugin worker (#2528): an `open-ui-link` chord opens the badge href (via
//! `AOE_OPEN_URL_TO`), and an action-less chord reaches the worker as
//! `plugin.command.invoke`. A plugin-supplied relative href (#4089) resolves
//! against the daemon's own base URL for both an `open-ui-link` command and a
//! `ui.open_url` notification, since the TUI has no browser origin of its own.

use std::path::Path;
use std::time::{Duration, Instant};

use serial_test::parallel;

use crate::harness::{require_node, require_tmux, TuiTestHarness};

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

    let (port, session_id) = h.start_structured_session("plugin-exec");
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
