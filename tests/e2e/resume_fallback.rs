use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};
use serial_test::parallel;

use crate::harness::{require_tmux, TuiTestHarness};

const TITLE: &str = "ResumeFallbackE2E";
const FAKE_AGENT: &str = "claude";
const STALE_SID: &str = "11111111-1111-4111-8111-111111111111";

fn new_harness(test_name: &str) -> TuiTestHarness {
    #[cfg(unix)]
    {
        TuiTestHarness::new_in_tmp(test_name)
    }
    #[cfg(not(unix))]
    {
        TuiTestHarness::new(test_name)
    }
}

fn sessions_path(h: &TuiTestHarness) -> PathBuf {
    crate::harness::app_dir_in(h.home_path()).join("profiles/default/sessions.json")
}

fn read_sessions(h: &TuiTestHarness) -> Value {
    let path = sessions_path(h);
    let content = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read {}: {}", path.display(), e));
    serde_json::from_str(&content).expect("invalid sessions JSON")
}

fn session_by_title<'a>(sessions: &'a Value, title: &str) -> &'a Value {
    sessions
        .as_array()
        .and_then(|arr| arr.iter().find(|s| s["title"].as_str() == Some(title)))
        .unwrap_or_else(|| panic!("no session titled '{title}' in sessions.json"))
}

fn patch_session<F>(h: &TuiTestHarness, title: &str, patch: F)
where
    F: FnOnce(&mut Map<String, Value>),
{
    let path = sessions_path(h);
    let mut sessions = read_sessions(h);
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

fn install_fake_agent(h: &mut TuiTestHarness) -> PathBuf {
    let bin = h.install_path_command(FAKE_AGENT);
    let log = h.home_path().join("resume-fallback-agent.log");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncase \"$*\" in\n  *{}*) exit 42 ;;\nesac\nexec sleep 30\n",
        sh_quote(&log),
        STALE_SID,
    );
    let script_path = bin.join(FAKE_AGENT);
    fs::write(&script_path, script).expect("write fake agent");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))
            .expect("chmod fake agent");
    }
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

/// Seed a Claude transcript on disk for `sid` so the Default resume path
/// attempts `--resume <sid>` (and fires the settle probe) instead of the
/// empty-thread fresh-pin shortcut from #2700, which launches fresh with
/// `--session-id` and skips the probe entirely. Mirrors the host location
/// `claude_host_transcript_confirmed_absent` checks: `$HOME/.claude/projects/
/// <encoded-canonical-project-path>/<sid>.jsonl`, where the encoding maps every
/// char that is not ASCII-alphanumeric or `-` to `-`. Models a real prior
/// Claude session whose sid later fails to resume.
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
fn migrated_unknown_restart_warns_and_leaves_the_old_conversation_intact() {
    require_tmux!();
    let mut h = new_harness("resume_unknown_warning");
    disable_restart_wake_message(&h);
    let log = install_fake_agent(&mut h);
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
    let diagnostic = String::from_utf8_lossy(&restarted.stderr);
    assert!(
        diagnostic.contains("starting fresh") && diagnostic.contains("unknown provenance"),
        "restart must explain why the previous conversation was not resumed: {restarted:?}"
    );
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let lines = loop {
        let lines = read_log_lines(&log);
        if !lines.is_empty() {
            break lines;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the agent was not invoked"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(
        lines.iter().all(|line| !line.contains(STALE_SID)),
        "{lines:?}"
    );
    let sessions = read_sessions(&h);
    let row = session_by_title(&sessions, TITLE);
    assert_ne!(row["agent_session_id"].as_str(), Some(STALE_SID));
    assert_default_resume_intent(row);
    assert_eq!(fs::read(&transcript).unwrap(), original);
}

#[test]
#[parallel]
fn migrated_unknown_start_warns_like_restart() {
    require_tmux!();
    let mut h = new_harness("resume_unknown_start_warning");
    let log = install_fake_agent(&mut h);
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
    let started = h.run_cli(&["session", "stop", TITLE]);
    assert!(started.status.success(), "{started:?}");
    let started = h.run_cli(&["session", "start", TITLE]);
    assert!(started.status.success(), "{started:?}");
    let diagnostic = String::from_utf8_lossy(&started.stderr);
    assert!(
        diagnostic.contains("starting fresh") && diagnostic.contains("unknown provenance"),
        "start must explain why the previous conversation was not resumed: {started:?}"
    );
    let sessions = read_sessions(&h);
    let row = session_by_title(&sessions, TITLE);
    assert_ne!(row["agent_session_id"].as_str(), Some(STALE_SID));
    assert_eq!(fs::read(&transcript).unwrap(), original);
}

#[test]
#[parallel]
fn stale_resume_failure_persists_loop_breaker_and_next_restart_starts_fresh() {
    require_tmux!();

    let mut h = new_harness("resume_fallback_loop_breaker");
    disable_restart_wake_message(&h);
    let log_path = install_fake_agent(&mut h);
    let project = h.project_path();

    let add = h.run_cli(&[
        "add",
        project.to_str().unwrap(),
        "--cmd",
        "claude",
        "-t",
        TITLE,
    ]);
    assert!(
        add.status.success(),
        "aoe add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let _cleanup = StopSessionOnDrop { h: &h };

    patch_session(&h, TITLE, |row| {
        row.insert("command".to_string(), Value::String(FAKE_AGENT.to_string()));
        row.insert("tool".to_string(), Value::String("claude".to_string()));
        row.insert("status".to_string(), Value::String("idle".to_string()));
        row.remove("resume_probe_failed_sid");
        row.remove("resume_intent");
    });

    // Without a transcript on disk, #2700 launches a stale Claude sid fresh
    // (`--session-id`, no probe), so the resume never fails and this test's
    // premise collapses. Seed one so the restart takes the `--resume` path.
    seed_claude_transcript(&h, &project, STALE_SID);

    // A migrated ID carries unknown provenance, which a managed launch refuses.
    // Assert the conversation through the documented path, then absorb that
    // resolved binding as the observation an automatic capture would have
    // recorded. The restart below is then the automatic `--resume` the loop
    // breaker governs, not an explicit pin.
    let asserted = h.run_cli(&["session", "set-session-id", TITLE, STALE_SID]);
    assert!(
        asserted.status.success(),
        "set-session-id failed: {}",
        String::from_utf8_lossy(&asserted.stderr)
    );
    patch_session(&h, TITLE, |row| {
        let binding = row["resume_binding"]
            .as_object()
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "set-session-id recorded no binding: {:?}",
                    row["resume_binding"]
                )
            });
        let mut binding = Value::Object(binding);
        binding["provenance"] = Value::String("observed".to_string());
        row.insert("agent_session_binding".to_string(), binding);
        row.insert(
            "agent_session_id".to_string(),
            Value::String(STALE_SID.to_string()),
        );
        row.remove("resume_intent");
        row.remove("resume_binding");
        row.remove("resume_probe_failed_sid");
    });

    let first = h.run_cli(&["session", "restart", TITLE]);
    assert!(
        !first.status.success(),
        "first restart should fail after passing stale sid: {}",
        String::from_utf8_lossy(&first.stderr)
    );

    let sessions = read_sessions(&h);
    let row = session_by_title(&sessions, TITLE);
    assert_eq!(row["agent_session_id"].as_str(), Some(STALE_SID));
    assert_eq!(row["resume_probe_failed_sid"].as_str(), Some(STALE_SID));
    assert_default_resume_intent(row);

    let first_lines = read_log_lines(&log_path);
    assert!(
        first_lines.iter().any(|line| line.contains(STALE_SID)),
        "first restart must pass stale sid to fake agent; log={first_lines:?}"
    );

    let before_second = first_lines.len();
    let second = h.run_cli(&["session", "restart", TITLE]);
    assert!(
        second.status.success(),
        "second restart should start fresh after loop-breaker: {}",
        String::from_utf8_lossy(&second.stderr)
    );

    let sessions = read_sessions(&h);
    let row = session_by_title(&sessions, TITLE);
    let fresh_sid = row["agent_session_id"]
        .as_str()
        .expect("fresh restart should persist a new agent_session_id");
    assert_ne!(fresh_sid, STALE_SID);
    assert!(!fresh_sid.trim().is_empty());
    assert!(
        row["resume_probe_failed_sid"].is_null(),
        "fresh restart should clear resume_probe_failed_sid, got {:?}",
        row["resume_probe_failed_sid"]
    );
    assert_default_resume_intent(row);

    let all_lines = read_log_lines(&log_path);
    let second_lines = &all_lines[before_second..];
    assert!(
        !second_lines.is_empty(),
        "second restart should invoke fake agent; log={all_lines:?}"
    );
    assert!(
        second_lines.iter().all(|line| !line.contains(STALE_SID)),
        "second restart must not retry stale sid; new log lines={second_lines:?}"
    );
}
