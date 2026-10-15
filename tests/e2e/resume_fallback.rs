use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use serial_test::parallel;

use crate::harness::{require_tmux, session_by_title, write_executable, TuiTestHarness};

const TITLE: &str = "ResumeFallbackE2E";
const FAKE_AGENT: &str = "claude";
const STALE_SID: &str = "11111111-1111-4111-8111-111111111111";

fn patch_session<F>(h: &TuiTestHarness, title: &str, patch: F)
where
    F: FnOnce(&mut Map<String, Value>),
{
    let path = h.sessions_path();
    let mut sessions = h.read_sessions();
    let row = sessions
        .as_array_mut()
        .and_then(|arr| arr.iter_mut().find(|s| s["title"].as_str() == Some(title)))
        .unwrap_or_else(|| panic!("no session titled '{title}' in sessions.json"));
    let row = row.as_object_mut().expect("session row must be an object");
    patch(row);
    fs::write(&path, serde_json::to_string_pretty(&sessions).unwrap())
        .unwrap_or_else(|e| panic!("failed to write {}: {}", path.display(), e));
}

fn assert_default_resume_intent(row: &Value) {
    let intent = &row["resume_intent"];
    assert!(
        intent.is_null() || intent["kind"].as_str() == Some("Default"),
        "resume_intent should be absent/null/default, got {intent:?}"
    );
}

fn install_fake_agent(h: &mut TuiTestHarness, reject_stale: bool) -> PathBuf {
    let bin = h.install_path_command(FAKE_AGENT);
    let log = h.home_path().join("resume-fallback-agent.log");
    let rejection = if reject_stale {
        format!("case \"$*\" in\n  *{STALE_SID}*) exit 42 ;;\nesac\n")
    } else {
        String::new()
    };
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\n{}exec sleep 30\n",
        sh_quote(&log),
        rejection,
    );
    write_executable(&bin.join(FAKE_AGENT), &script);
    log
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn disable_restart_wake_message(h: &TuiTestHarness) {
    let config_path = crate::harness::app_dir_in(h.home_path()).join("config.toml");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&config_path)
        .unwrap_or_else(|e| panic!("failed to open {}: {}", config_path.display(), e));
    file.write_all(b"\n[session]\nrestart_wake_message = \"\"\n")
        .expect("disable restart wake message");
}

fn read_log_lines(path: &Path) -> Vec<String> {
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn wait_for_logged_args(path: &Path, sid: &str) -> Vec<String> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let lines = read_log_lines(path);
        if lines.iter().any(|line| line.contains(sid)) {
            return lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "agent never received {sid}: {lines:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// Seed a Claude transcript at `$HOME/.claude/projects/<encoded project>/
/// <sid>.jsonl` (every char outside `[A-Za-z0-9-]` maps to `-`) so the restart
/// takes the `--resume <sid>` path instead of #2700's fresh-pin shortcut.
fn seed_claude_transcript(h: &TuiTestHarness, project_path: &Path, sid: &str) -> PathBuf {
    let canonical = fs::canonicalize(project_path).unwrap_or_else(|_| project_path.to_path_buf());
    let encoded: String = canonical
        .to_string_lossy()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let dir = h.home_path().join(".claude").join("projects").join(encoded);
    fs::create_dir_all(&dir).expect("create claude projects dir");
    let path = dir.join(format!("{sid}.jsonl"));
    fs::write(&path, "{}\n").expect("write claude transcript");
    path
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
#[parallel]
fn migrated_unknown_restart_resumes_the_stored_conversation() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("resume_unknown_restart");
    disable_restart_wake_message(&h);
    let log = install_fake_agent(&mut h, false);
    let project = h.project_path();
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        FAKE_AGENT,
        "-t",
        TITLE,
    ]);
    assert!(add.status.success(), "{add:?}");
    let _cleanup = StopSessionOnDrop { h: &h };
    let transcript = seed_claude_transcript(&h, &project, STALE_SID);
    let original = fs::read(&transcript).unwrap();
    patch_session(&h, TITLE, |row| {
        row.insert("agent_session_id".into(), Value::String(STALE_SID.into()));
        row.insert(
            "agent_session_binding".into(),
            serde_json::json!({
                "session_id": STALE_SID,
                "execution": null,
                "provenance": "unknown",
                "transcript_path": null
            }),
        );
        row.remove("resume_intent");
        row.remove("resume_binding");
        row.remove("active_execution");
    });
    let restarted = h.run_cli(&["session", "restart", TITLE]);
    assert!(restarted.status.success(), "{restarted:?}");
    let lines = wait_for_logged_args(&log, STALE_SID);
    assert!(
        lines
            .iter()
            .any(|line| line.contains("--resume") && line.contains(STALE_SID)),
        "restart must pass native resume flags: {lines:?}"
    );
    let sessions = h.read_sessions();
    let row = session_by_title(&sessions, TITLE);
    assert_eq!(row["agent_session_id"].as_str(), Some(STALE_SID));
    assert_default_resume_intent(row);
    assert_eq!(fs::read(&transcript).unwrap(), original);
}

#[test]
#[parallel]
fn migrated_unknown_start_resumes_without_attested_context() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("resume_unknown_start");
    let log = install_fake_agent(&mut h, false);
    let project = h.project_path();
    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        FAKE_AGENT,
        "-t",
        TITLE,
    ]);
    assert!(add.status.success(), "{add:?}");
    let _cleanup = StopSessionOnDrop { h: &h };
    let stopped = h.run_cli(&["session", "stop", TITLE]);
    assert!(stopped.status.success(), "{stopped:?}");
    let transcript = seed_claude_transcript(&h, &project, STALE_SID);
    let original = fs::read(&transcript).unwrap();
    patch_session(&h, TITLE, |row| {
        row.insert(
            "extra_args".into(),
            Value::String("--mcp-config /tmp/unattested.json".into()),
        );
        row.insert("agent_session_id".into(), Value::String(STALE_SID.into()));
        row.insert(
            "agent_session_binding".into(),
            serde_json::json!({
                "session_id": STALE_SID,
                "execution": null,
                "provenance": "unknown",
                "transcript_path": null
            }),
        );
        row.remove("resume_intent");
        row.remove("resume_binding");
        row.remove("active_execution");
    });
    let started = h.run_cli(&["session", "start", TITLE]);
    assert!(started.status.success(), "{started:?}");
    let lines = wait_for_logged_args(&log, STALE_SID);
    assert!(
        lines
            .iter()
            .any(|line| line.contains(STALE_SID) && line.contains("--mcp-config")),
        "unattested launch must still try the stored ID: {lines:?}"
    );
    let sessions = h.read_sessions();
    let row = session_by_title(&sessions, TITLE);
    assert_eq!(row["agent_session_id"].as_str(), Some(STALE_SID));
    assert_eq!(fs::read(&transcript).unwrap(), original);
}
