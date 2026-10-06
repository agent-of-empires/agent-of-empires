//! Native Pi fork dispatch, child identity persistence, and fail-closed launches.

use std::path::PathBuf;
use std::time::Duration;

use serde_json::{json, Value};
use serial_test::parallel;

use crate::harness::{
    require_tmux, session_by_title, wait_until, write_executable, TuiTestHarness,
};

const PARENT_ID: &str = "11111111-2222-4333-8444-555555555555";

fn pi_parent(h: &mut TuiTestHarness, help: &str) -> PathBuf {
    let store = h.home_path().join("pi sessions");
    std::fs::create_dir_all(&store).unwrap();
    h.set_env("PI_CODING_AGENT_SESSION_DIR", store.to_str().unwrap());
    h.set_env(
        "PI_CODING_AGENT_DIR",
        h.home_path().join(".pi/agent").to_str().unwrap(),
    );
    let bin = h.install_path_command("pi");
    write_executable(
        &bin.join("pi"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = --help ]; then\n  printf '%s\\n' {}\n  exit 0\nfi\n{}",
            shell_words::quote(help),
            r#"set -eu
mkdir -p "$HOME/pi-launches"
record="$HOME/pi-launches/$$"
printf '%s\n' "$@" > "$record.tmp"
parent= child= session=
store="$PI_CODING_AGENT_SESSION_DIR"
while [ "$#" -gt 0 ]; do
  case "$1" in
    --fork) parent="$2"; shift ;;
    --session-id) child="$2"; shift ;;
    --session) session="$2"; shift ;;
    --session-dir) store="$2"; shift ;;
  esac
  shift
done
if [ -n "$parent" ]; then
  file="$store/fork_$child.jsonl"
  printf '{"type":"session","version":3,"id":"%s","cwd":"%s"}\n' "$child" "$PWD" > "$file"
  tail -n +2 "$parent" >> "$file"
else
  file="$session"
  [ -f "$file" ] || exit 1
  child=$(sed -n '1s/.*"id":"\([^"]*\)".*/\1/p' "$file")
fi
if [ -n "${AOE_SESSION_ID_FILE:-}" ]; then
  suffix="${AOE_SESSION_SOURCE:+.$AOE_SESSION_SOURCE}"
  printf '%s' "$child" > "$AOE_SESSION_ID_FILE$suffix"
  printf '%s' "$file" > "$(dirname "$AOE_SESSION_ID_FILE")/session_path$suffix"
fi
mv "$record.tmp" "$record.argv"
exec sleep 600
"#
        ),
    );
    let project = h.project_path();
    let transcript = store.join(format!("parent_{PARENT_ID}.jsonl"));
    let header = json!({
        "type": "session", "version": 3, "id": PARENT_ID,
        "cwd": project.to_str().unwrap(),
    });
    std::fs::write(
        &transcript,
        format!("{header}\n{}\n", json!({"type": "message", "id": "saved-message", "message": {"role": "user", "content": [{"type": "text", "text": "saved parent context"}]}})),
    ).unwrap();
    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "pi",
        "-t",
        "PiParent",
    ]);
    h.run_cli_ok(&[
        "session",
        "set-session-id",
        "PiParent",
        PARENT_ID,
        "--store",
        transcript.to_str().unwrap(),
    ]);
    transcript
}

fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_str())
}

fn launches(h: &TuiTestHarness, count: usize) -> Vec<Vec<String>> {
    wait_until(Duration::from_secs(20), Duration::from_millis(50), || {
        let launches: Vec<Vec<String>> = std::fs::read_dir(h.home_path().join("pi-launches"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "argv"))
            .map(|entry| {
                std::fs::read_to_string(entry.path())
                    .unwrap()
                    .lines()
                    .map(str::to_owned)
                    .collect()
            })
            .collect();
        if launches.len() == count {
            Ok(launches)
        } else {
            Err(format!(
                "expected {count} complete Pi launches, got {launches:?}"
            ))
        }
    })
}

fn child_id(sessions: &Value, title: &str) -> String {
    session_by_title(sessions, title)["agent_session_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
#[parallel]
fn pi_fork_keeps_distinct_child_ids_and_restarts_without_reforking() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("pi_fork_restart");
    let transcript = pi_parent(
        &mut h,
        "--fork <path|id>\n--session-id <id>\n--extension <path>\n--session <path|id>",
    );
    let parent_bytes = std::fs::read(&transcript).unwrap();
    let parent_row = session_by_title(&h.read_sessions(), "PiParent").clone();
    let project = h.project_path();
    let mut ids = Vec::new();
    for (index, title) in ["PiChildA", "PiChildB"].into_iter().enumerate() {
        h.run_cli_ok(&[
            "add",
            project.to_str().unwrap(),
            "-t",
            title,
            "--fork-from",
            "PiParent",
        ]);
        let sessions = h.read_sessions();
        let child = session_by_title(&sessions, title);
        let id = child_id(&sessions, title);
        assert_eq!(child["tool"], "pi");
        assert_eq!(child["resume_intent"]["kind"], "Fork");
        assert_ne!(id, PARENT_ID);
        assert!(!ids.contains(&id));
        h.run_cli_ok(&["session", "start", title]);
        let recorded = launches(&h, index + 1);
        let fork = recorded
            .iter()
            .find(|argv| flag_value(argv, "--session-id") == Some(id.as_str()))
            .unwrap();
        assert_eq!(
            std::fs::canonicalize(flag_value(fork, "--fork").unwrap()).unwrap(),
            std::fs::canonicalize(&transcript).unwrap()
        );
        assert_eq!(flag_value(fork, "--session"), None);
        assert_eq!(
            child_id(&h.read_sessions(), title),
            id,
            "launch must keep the reserved child id"
        );
        assert!(
            session_by_title(&h.read_sessions(), title)
                .get("resume_intent")
                .is_none(),
            "the one-shot fork intent must be consumed"
        );
        ids.push(id);
    }
    h.run_cli_ok(&["session", "stop", "PiChildA"]);
    h.run_cli_ok(&["session", "start", "PiChildA"]);
    let recorded = launches(&h, 3);
    let resume = recorded
        .iter()
        .find(|argv| flag_value(argv, "--session").is_some())
        .unwrap();
    assert_eq!(flag_value(resume, "--fork"), None);
    let file = PathBuf::from(flag_value(resume, "--session").unwrap());
    let header: Value = serde_json::from_str(
        std::fs::read_to_string(&file)
            .unwrap()
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(header["id"], ids[0]);
    let sessions = h.read_sessions();
    assert_eq!(child_id(&sessions, "PiChildA"), ids[0]);
    assert_eq!(child_id(&sessions, "PiChildB"), ids[1]);
    assert_eq!(session_by_title(&sessions, "PiParent"), &parent_row);
    assert_eq!(std::fs::read(&transcript).unwrap(), parent_bytes);
}

#[test]
#[parallel]
fn pi_fork_refuses_unpinnable_launches_without_changing_the_seed() {
    require_tmux!();
    for (case, help, command, routed_path) in [
        (
            "no_fork",
            "--session-id <id>\n--session <path|id>",
            None,
            false,
        ),
        (
            "no_pin",
            "--fork <path|id>\n--session <path|id>",
            None,
            false,
        ),
        (
            "wrong_flag",
            "--forked <id>\n--session-id <id>\n--session <path|id>",
            None,
            false,
        ),
        (
            "wrapper",
            "--fork <path|id>\n--session-id <id>",
            Some("sh -c pi"),
            false,
        ),
        (
            "routed_path",
            "--fork <path|id>\n--session-id <id>",
            None,
            true,
        ),
    ] {
        let mut h = TuiTestHarness::new_in_tmp(&format!("pi_fork_{case}"));
        let transcript = pi_parent(&mut h, help);
        let bytes = std::fs::read(&transcript).unwrap();
        if routed_path {
            let path = format!("PATH={}/path-bin:/usr/bin:/bin", h.home_path().display());
            let config = crate::harness::app_dir_in(h.home_path()).join("config.toml");
            let seeded = std::fs::read_to_string(&config).unwrap();
            std::fs::write(
                config,
                format!(
                    "environment = [{}]\n{seeded}",
                    serde_json::to_string(&path).unwrap()
                ),
            )
            .unwrap();
        }
        let project = h.project_path();
        let mut args = vec![
            "add",
            project.to_str().unwrap(),
            "-t",
            "PiChild",
            "--fork-from",
            "PiParent",
        ];
        if let Some(command) = command {
            args.extend(["--cmd", command]);
        }
        h.run_cli_ok(&args);
        let before = h.read_sessions();
        let output = h.run_cli(&["session", "start", "PiChild"]);
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            !output.status.success(),
            "{case}: launch must be refused, stderr: {error}"
        );
        if command.is_none() {
            assert!(error.contains("Pi fork needs"), "{case}: {error}");
        }
        assert!(
            !h.home_path().join("pi-launches").exists(),
            "{case}: Pi must not dispatch"
        );
        let after = h.read_sessions();
        for field in [
            "agent_session_id",
            "agent_session_binding",
            "resume_intent",
            "resume_binding",
            "active_execution",
            "pi_session_path",
        ] {
            assert_eq!(
                session_by_title(&before, "PiChild")[field],
                session_by_title(&after, "PiChild")[field],
                "{case}: refusal changed {field}"
            );
        }
        assert_eq!(
            session_by_title(&before, "PiParent"),
            session_by_title(&after, "PiParent")
        );
        assert_eq!(std::fs::read(&transcript).unwrap(), bytes);
    }
}
