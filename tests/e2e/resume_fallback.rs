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

/// A stored transcript makes this a resume rather than an empty-thread fresh pin.
fn seed_claude_transcript(h: &TuiTestHarness, project_path: &Path, sid: &str) {
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
    fs::write(dir.join(format!("{sid}.jsonl")), "{}\n").expect("write claude transcript");
}

struct StopSessionOnDrop<'a> {
    h: &'a TuiTestHarness,
}

impl Drop for StopSessionOnDrop<'_> {
    fn drop(&mut self) {
        let _ = self.h.run_cli(&["session", "stop", TITLE]);
    }
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
            row.insert("command".into(), Value::String(FAKE_AGENT.into()));
            row.insert("tool".into(), Value::String("claude".into()));
            // Exclude daemon startup recovery from this explicit request.
            row.insert("status".into(), Value::String("stopped".into()));
            row.insert("agent_session_id".into(), Value::String(STALE_SID.into()));
            row.remove("resume_probe_failed_sid");
            row.remove("resume_intent");
        });
        seed_claude_transcript(&h, &project, STALE_SID);
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
        let all_lines = read_log_lines(&log_path);
        let second_lines = &all_lines[before_second..];
        assert_eq!(
            second_lines.len(),
            1,
            "explicit retry should launch exactly once; log={all_lines:?}"
        );
        assert!(
            !second_lines[0].contains(STALE_SID),
            "explicit retry must not reuse stale sid; log={second_lines:?}"
        );
    }
}
