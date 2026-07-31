//! E2E: a CLI-launched, capture-deferred agent must persist
//! `agent_session_id` with no `aoe serve` daemon and no TUI running (#3169).
//!
//! Uses a fake `codex` (a capture-deferred agent) that, on launch, writes a
//! rollout file the codex poller scans, then idles so its tmux pane stays
//! alive. Before the fix, `aoe session start` returned before draining the
//! poller, so the observed id was dropped and `sessions.json` kept
//! `agent_session_id: null`, silently breaking resume.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use serial_test::serial;

use crate::harness::{app_dir_in, require_tmux, TuiTestHarness};

const TITLE: &str = "CliSidCaptureE2E";
const FAKE_SID: &str = "019342ab-1234-7def-8901-abcdef012345";

fn new_harness(name: &str) -> TuiTestHarness {
    #[cfg(unix)]
    {
        TuiTestHarness::new_in_tmp(name)
    }
    #[cfg(not(unix))]
    {
        TuiTestHarness::new(name)
    }
}

fn sessions_path(h: &TuiTestHarness) -> PathBuf {
    app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn agent_session_id(h: &TuiTestHarness, title: &str) -> Option<String> {
    let content = fs::read_to_string(sessions_path(h)).ok()?;
    let sessions: Value = serde_json::from_str(&content).ok()?;
    sessions
        .as_array()?
        .iter()
        .find(|s| s["title"].as_str() == Some(title))?
        .get("agent_session_id")?
        .as_str()
        .map(str::to_owned)
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn install_fake_codex(h: &mut TuiTestHarness, codex_home: &Path, project: &Path) {
    let bin = h.install_path_command("codex");
    let sessions_dir = codex_home.join("sessions");
    let rollout = sessions_dir.join(format!("rollout-2025-01-01T00-00-00-{FAKE_SID}.jsonl"));
    let script = format!(
        "#!/bin/sh\nmkdir -p {dir}\nprintf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {file}\nexec sleep 300\n",
        dir = sh_quote(&sessions_dir),
        cwd = sh_quote(project),
        file = sh_quote(&rollout),
    );
    let script_path = bin.join("codex");
    fs::write(&script_path, script).expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod codex");
    }
}

struct StopSessionOnDrop<'a> {
    h: &'a TuiTestHarness,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", TITLE]);
    }
}

#[test]
#[serial]
fn cli_session_start_persists_agent_session_id_without_daemon() {
    require_tmux!();
    let mut h = new_harness("cli_sid_capture");
    let project = h.project_path();
    let codex_home = h.home_path().join("codex-home");
    fs::create_dir_all(&codex_home).expect("create codex home");
    h.set_env("CODEX_HOME", codex_home.to_str().expect("utf8 codex home"));
    install_fake_codex(&mut h, &codex_home, &project);

    let add = h.run_cli(&["add", project.to_str().unwrap(), "-c", "codex", "-t", TITLE]);
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    assert_eq!(
        agent_session_id(&h, TITLE),
        None,
        "agent_session_id must be unset before launch"
    );

    let _stop = StopSessionOnDrop { h: &h };
    let start = h.run_cli(&["session", "start", TITLE]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    assert_eq!(
        agent_session_id(&h, TITLE).as_deref(),
        Some(FAKE_SID),
        "#3169: CLI launch must drain the poller and persist agent_session_id \
         without a daemon or TUI"
    );
}
