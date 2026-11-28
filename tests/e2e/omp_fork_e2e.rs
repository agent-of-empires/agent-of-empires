//! Native OMP fork dispatch through `omp --fork`, with the parent binding a
//! declared plugin publishes rather than a capture of the pane.
//!
//! Core no longer reads an OMP conversation out of the pane, so the parent row
//! here carries the binding the plugin sidecar would publish. What this file
//! still covers is the fork launch itself: the one-shot intent becomes
//! `omp --resume <parent> --fork` pinned to the parent's session directory, and
//! a restart does not fork again.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serial_test::parallel;

use crate::harness::{
    install_path_preserving_test_shell, require_tmux, session_by_title, wait_until,
    write_executable, TuiTestHarness,
};

const PARENT_ID: &str = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
const CHILD_ID: &str = "019342ab-1234-7def-8901-bbbbbbbbbbbb";

/// A fake `omp` that keeps the store contract AoE relies on: a fresh launch
/// creates a session, `--fork <id>` copies that session from `--session-dir`
/// into a new one there, and `--resume <id>` reopens it. Each launch records
/// its argv.
fn install_fake_omp(h: &mut TuiTestHarness, project: &Path) {
    let bin = h.install_path_command("omp");
    install_path_preserving_test_shell(h, &bin);
    let script = r#"#!/bin/sh
set -eu
[ "${1-}" = --help ] && exit 0
launches="$HOME/omp-launches"
mkdir -p "$launches"
printf '%s\n' "$@" > "$launches/$$.tmp"
store="$HOME/.omp/agent"
dir="$store/sessions/home-project"
fork= resume=
while [ "$#" -gt 0 ]; do
  case "$1" in
    --fork) fork="$2"; shift ;;
    --resume) resume="$2"; shift ;;
    --session-dir) dir="$2"; shift ;;
  esac
  shift
done
mkdir -p "$dir" "$store/terminal-sessions"
if [ -n "$resume" ]; then
  file=$(ls "$dir"/*_"$resume".jsonl)
else
  parent=
  if [ -n "$fork" ]; then
    parent=$(ls "$dir"/*_"$fork".jsonl)
    sid=CHILD
  else
    sid=PARENT
  fi
  file="$dir/2026-08-05T00-00-00-000Z_$sid.jsonl"
  printf '{"type":"title","v":1,"title":"Fake OMP fork","source":"user","updatedAt":"2026-08-05T00:00:00.000Z","pad":""}\n' > "$file"
  printf '{"type":"session","version":3,"id":"%s","timestamp":"2026-08-05T00:00:00.000Z","cwd":"%s","parentSession":"%s"}\n' "$sid" CWD "$fork" >> "$file"
  if [ -n "$parent" ]; then
    tail -n +3 "$parent" >> "$file"
  else
    printf '{"type":"message","id":"saved-message","message":{"role":"user","content":[{"type":"text","text":"saved parent context"}]}}\n' >> "$file"
  fi
fi
tty_path=$(tty)
terminal_id=$(printf '%s' "${tty_path#/dev/}" | tr '/' '-')
printf '%s\n%s\n' CWD "$file" > "$store/terminal-sessions/$terminal_id"
mv "$launches/$$.tmp" "$launches/$$.argv"
exec sleep 300
"#.replace("CHILD", CHILD_ID)
        .replace("PARENT", PARENT_ID)
        .replace("CWD", project.to_str().expect("UTF-8 project path"));
    write_executable(&bin.join("omp"), &script);
}

/// Wait for the one launch not in `seen`, record it there, and return its argv.
fn next_launch(h: &TuiTestHarness, seen: &mut BTreeSet<PathBuf>) -> Vec<String> {
    let record = wait_until(Duration::from_secs(20), Duration::from_millis(50), || {
        let new: Vec<PathBuf> = std::fs::read_dir(h.home_path().join("omp-launches"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "argv"))
            .filter(|path| !seen.contains(path))
            .collect();
        match new.as_slice() {
            [record] => Ok(record.clone()),
            _ => Err(format!("expected one new OMP launch, got {new:?}")),
        }
    });
    let argv = std::fs::read_to_string(&record)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    seen.insert(record);
    argv
}

fn flag_value<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
    argv.windows(2)
        .find(|pair| pair[0] == flag)
        .map(|pair| pair[1].as_str())
}

fn wait_for_file(path: &Path) {
    wait_until(Duration::from_secs(15), Duration::from_millis(50), || {
        path.is_file()
            .then_some(())
            .ok_or_else(|| format!("{} was never written", path.display()))
    });
}

/// Publish the binding a declared OMP plugin's sidecar writes for `sid`: the
/// conversation id, the execution the launch recorded, and its transcript.
fn publish_parent_binding(h: &TuiTestHarness, title: &str, sid: &str, transcript: &Path) {
    let transcript = std::fs::canonicalize(transcript).expect("canonical transcript path");
    let mut sessions = h.read_sessions();
    let row = sessions
        .as_array_mut()
        .expect("sessions file is an array")
        .iter_mut()
        .find(|row| row["title"].as_str() == Some(title))
        .expect("parent row exists");
    let execution = row["active_execution"]["binding"].clone();
    assert!(
        !execution.is_null(),
        "the parent launch must record the execution its binding belongs to"
    );
    row["agent_session_id"] = serde_json::json!(sid);
    row["agent_session_binding"] = serde_json::json!({
        "session_id": sid,
        "execution": execution,
        "provenance": "observed",
        "transcript_path": transcript,
    });
    std::fs::write(
        h.sessions_path(),
        serde_json::to_string_pretty(&sessions).unwrap(),
    )
    .expect("write sessions.json");
}

#[test]
#[parallel]
fn omp_fork_dispatches_the_native_flag_and_does_not_fork_again_on_restart() {
    require_tmux!();
    let mut h = TuiTestHarness::new_in_tmp("omp_fork_restart");
    let project = h.project_path();
    install_fake_omp(&mut h, &project);

    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "--tool",
        "omp",
        "-t",
        "OmpParent",
    ]);
    h.run_cli_ok(&["session", "start", "OmpParent"]);
    let mut seen = BTreeSet::new();
    next_launch(&h, &mut seen);

    let parent_transcript = h
        .home_path()
        .join(".omp/agent/sessions/home-project")
        .join(format!("2026-08-05T00-00-00-000Z_{PARENT_ID}.jsonl"));
    wait_for_file(&parent_transcript);
    publish_parent_binding(&h, "OmpParent", PARENT_ID, &parent_transcript);
    let published =
        session_by_title(&h.read_sessions(), "OmpParent")["agent_session_binding"].clone();

    h.run_cli_ok(&[
        "add",
        project.to_str().unwrap(),
        "-t",
        "OmpChild",
        "--fork-from",
        "OmpParent",
    ]);
    let sessions = h.read_sessions();
    let child = session_by_title(&sessions, "OmpChild");
    assert_eq!(child["tool"], "omp");
    assert_eq!(child["resume_intent"]["kind"], "Fork");

    h.run_cli_ok(&["session", "start", "OmpChild"]);
    let fork = &next_launch(&h, &mut seen);
    assert_eq!(flag_value(fork, "--fork"), Some(PARENT_ID));
    assert_eq!(
        std::fs::canonicalize(flag_value(fork, "--session-dir").unwrap()).unwrap(),
        std::fs::canonicalize(parent_transcript.parent().unwrap()).unwrap(),
        "the fork must be pinned to the parent's session directory"
    );
    assert!(
        session_by_title(&h.read_sessions(), "OmpChild")
            .get("resume_intent")
            .is_none(),
        "the one-shot fork intent must be consumed"
    );

    h.run_cli_ok(&["session", "stop", "OmpChild"]);
    h.run_cli_ok(&["session", "start", "OmpChild"]);
    let restart = &next_launch(&h, &mut seen);
    assert_eq!(
        flag_value(restart, "--fork"),
        None,
        "a restarted child must not fork the parent again"
    );

    assert_eq!(
        session_by_title(&h.read_sessions(), "OmpParent")["agent_session_binding"],
        published,
        "the fork must leave the parent's binding alone"
    );
}
