//! E2E: a CLI-launched, capture-deferred agent must persist
//! `agent_session_id` with no `aoe serve` daemon and no TUI running.
//!
//! Uses a fake `codex` (a capture-deferred agent) that, on launch, writes a
//! rollout file the codex poller scans, then idles so its tmux pane stays
//! alive. Before the fix, `aoe session start` returned before draining the
//! poller, so the observed id was dropped and `sessions.json` kept
//! `agent_session_id: null`, silently breaking resume.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;
use serial_test::parallel;

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
    title: &'a str,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", self.title]);
    }
}

#[test]
#[parallel]
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

    let _stop = StopSessionOnDrop {
        h: &h,
        title: TITLE,
    };
    let start = h.run_cli(&["session", "start", TITLE]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );

    assert_eq!(
        agent_session_id(&h, TITLE).as_deref(),
        Some(FAKE_SID),
        "CLI launch must drain the poller and persist agent_session_id \
         without a daemon or TUI"
    );
}

const RESTART_TITLE: &str = "CliSidRestartE2E";
const RESTART_SID_FIRST: &str = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
const RESTART_SID_SECOND: &str = "019342ab-1234-7def-8901-bbbbbbbbbbbb";

/// Fake codex that mints `RESTART_SID_FIRST` on its first launch and
/// `RESTART_SID_SECOND` on every launch after (tracked by a marker file). The
/// codex poller picks the newest rollout by mtime. On restart the second
/// rollout is written after a 1s delay, so the freshly-started poller's
/// immediate first poll sees only the lingering first rollout: without the
/// forced-fresh exclusion the restart drain would capture and persist the
/// stale `RESTART_SID_FIRST` and stop waiting; the exclusion makes it reject
/// that and wait for the second rollout, which lands well inside the bound.
fn install_toggling_fake_codex(h: &mut TuiTestHarness, codex_home: &Path, project: &Path) {
    let bin = h.install_path_command("codex");
    let sessions_dir = codex_home.join("sessions");
    let marker = codex_home.join(".launched");
    let rollout_first = sessions_dir.join(format!(
        "rollout-2025-01-01T00-00-00-{RESTART_SID_FIRST}.jsonl"
    ));
    let rollout_second = sessions_dir.join(format!(
        "rollout-2025-01-02T00-00-00-{RESTART_SID_SECOND}.jsonl"
    ));
    let script = format!(
        "#!/bin/sh\nmkdir -p {dir}\nif [ -f {marker} ]; then\n  sleep 1\n  \
         printf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {second}\nelse\n  \
         : > {marker}\n  printf '{{\"payload\":{{\"cwd\":\"%s\"}}}}\\n' {cwd} > {first}\nfi\n\
         exec sleep 300\n",
        dir = sh_quote(&sessions_dir),
        marker = sh_quote(&marker),
        cwd = sh_quote(project),
        first = sh_quote(&rollout_first),
        second = sh_quote(&rollout_second),
    );
    let script_path = bin.join("codex");
    fs::write(&script_path, script).expect("write fake codex");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755)).expect("chmod codex");
    }
}

/// Force `session restart` down the fresh-launch path (no `--resume <sid>`), so
/// the restarted agent mints a new capture-deferred sid the restart path must
/// drain. Without `auto_resume_on_restart = false` the restart resumes the
/// existing sid and never observes a new one.
///
/// Also blanks `restart_wake_message`, which removes the pre-capture
/// `wait_for_pane_ready` wake wait. Without that, the capture helper drains the
/// fresh poller immediately after relaunch, while only the lingering first
/// rollout exists (the second is written 1s later), so the forced-fresh
/// exclusion is load-bearing: without it the drain persists the stale
/// `RESTART_SID_FIRST` and returns. With the wake wait in place the poller
/// would observe both rollouts before the drain and pick the newest anyway,
/// masking the exclusion.
fn configure_fresh_restart_capture(h: &TuiTestHarness) {
    let config = app_dir_in(h.home_path()).join("config.toml");
    let mut doc = fs::read_to_string(&config)
        .unwrap_or_default()
        .parse::<toml_edit::DocumentMut>()
        .expect("parse config.toml");
    doc["session"]["auto_resume_on_restart"] = toml_edit::value(false);
    doc["session"]["restart_wake_message"] = toml_edit::value("");
    fs::write(&config, doc.to_string()).expect("write config.toml");
}

#[test]
#[parallel]
fn cli_session_restart_persists_new_agent_session_id_without_daemon() {
    require_tmux!();
    let mut h = new_harness("cli_sid_restart");
    let project = h.project_path();
    let codex_home = h.home_path().join("codex-home");
    fs::create_dir_all(&codex_home).expect("create codex home");
    h.set_env("CODEX_HOME", codex_home.to_str().expect("utf8 codex home"));
    install_toggling_fake_codex(&mut h, &codex_home, &project);
    configure_fresh_restart_capture(&h);

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "-c",
        "codex",
        "-t",
        RESTART_TITLE,
    ]);
    assert!(
        add.status.success(),
        "add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let _stop = StopSessionOnDrop {
        h: &h,
        title: RESTART_TITLE,
    };
    let start = h.run_cli(&["session", "start", RESTART_TITLE]);
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert_eq!(
        agent_session_id(&h, RESTART_TITLE).as_deref(),
        Some(RESTART_SID_FIRST),
        "start must capture the first sid"
    );

    let restart = h.run_cli(&["session", "restart", RESTART_TITLE]);
    assert!(
        restart.status.success(),
        "restart failed: {}",
        String::from_utf8_lossy(&restart.stderr)
    );
    assert_eq!(
        agent_session_id(&h, RESTART_TITLE).as_deref(),
        Some(RESTART_SID_SECOND),
        "`session restart` must drain the fresh poller and persist the new \
         agent_session_id without a daemon or TUI"
    );
}
