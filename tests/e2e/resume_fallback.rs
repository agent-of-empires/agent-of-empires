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

/// A stored transcript makes this a resume rather than an empty-thread fresh pin.
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

#[tokio::test]
#[parallel]
async fn stale_resume_failure_persists_loop_breaker_and_next_restart_starts_fresh() {
    use agent_of_empires::daemon::{ApiErrorCode, DaemonClient, DaemonClientError};
    require_tmux!();
    for native in [false, true] {
        let mut h = new_harness("resume_fallback_loop_breaker");
        h.set_env("AGENT_OF_EMPIRES_PROFILE", "default");
        if native {
            h.stop_daemon_on_drop();
        }
        disable_restart_wake_message(&h);
        let log_path = install_fake_agent(&mut h, true);
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
            // Exclude daemon startup recovery from this explicit request.
            row.insert("status".to_string(), Value::String("stopped".to_string()));
            row.insert(
                "agent_session_id".to_string(),
                Value::String(STALE_SID.to_string()),
            );
            row.remove("resume_probe_failed_sid");
            row.remove("resume_intent");
        });
        seed_claude_transcript(&h, &project, STALE_SID);
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
        let runtime = if native {
            let started = h.run_cli(&["serve", "--core-only", "--daemon"]);
            assert!(
                started.status.success(),
                "{}",
                String::from_utf8_lossy(&started.stderr)
            );
            let sdk = DaemonClient::new_unix(
                crate::harness::app_dir_in(h.home_path()).join("daemon/api.sock"),
            )
            .unwrap();
            let epoch = sdk.runtime_info().await.unwrap().epoch;
            Some((sdk, epoch))
        } else {
            None
        };
        let id = session_by_title(&read_sessions(&h), TITLE)["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let first_succeeded = if let Some((sdk, epoch)) = &runtime {
            match sdk.ensure_agent(&id, &Default::default(), epoch).await {
                Err(DaemonClientError::Status {
                    code: Some(ApiErrorCode::ResumeFailed),
                    body,
                    ..
                }) => {
                    assert!(
                        body.is_empty(),
                        "authenticated error bodies must remain redacted"
                    );
                    false
                }
                Err(error) => panic!("resume failed without its typed discriminator: {error}"),
                Ok(_) => true,
            }
        } else {
            h.run_cli(&["session", "restart", TITLE]).status.success()
        };
        assert!(
            !first_succeeded,
            "first restart should fail after passing stale sid; native={native}; rows={}; log={:?}",
            read_sessions(&h),
            read_log_lines(&log_path)
        );
        let sessions = read_sessions(&h);
        let row = session_by_title(&sessions, TITLE);
        assert_eq!(row["agent_session_id"].as_str(), Some(STALE_SID));
        assert_eq!(row["resume_probe_failed_sid"].as_str(), Some(STALE_SID));
        assert_default_resume_intent(row);
        let first_lines = read_log_lines(&log_path);
        assert_eq!(
            first_lines.len(),
            1,
            "failed resume must not start fresh automatically"
        );
        assert!(
            first_lines[0].contains(STALE_SID),
            "first restart must pass stale sid; log={first_lines:?}"
        );
        let before_second = first_lines.len();
        let second = if let Some((sdk, epoch)) = &runtime {
            sdk.ensure_agent(&id, &Default::default(), epoch)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        } else {
            let output = h.run_cli(&["session", "restart", TITLE]);
            output
                .status
                .success()
                .then_some(())
                .ok_or_else(|| String::from_utf8_lossy(&output.stderr).into_owned())
        };
        assert!(
            second.is_ok(),
            "explicit retry should start fresh after the loop-breaker: {second:?}"
        );
        let sessions = read_sessions(&h);
        let row = session_by_title(&sessions, TITLE);
        let fresh_sid = row["agent_session_id"]
            .as_str()
            .expect("fresh restart should persist a new agent_session_id");
        assert_ne!(fresh_sid, STALE_SID);
        assert!(
            row["resume_probe_failed_sid"].is_null(),
            "fresh restart should clear the resume loop-breaker"
        );
        assert_default_resume_intent(row);
        // The CLI returns once the poller drain lands; the pane shell may still
        // be a few milliseconds from the fake agent's printf, so read until the
        // invocation appears instead of sampling the log once (races under load).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let all_lines = read_log_lines(&log_path);
            let second_lines = &all_lines[before_second..];
            assert!(
                second_lines.iter().all(|line| !line.contains(STALE_SID)),
                "second restart must not retry stale sid; new log lines={second_lines:?}"
            );
            if !second_lines.is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "second restart should invoke fake agent; log={all_lines:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
}
