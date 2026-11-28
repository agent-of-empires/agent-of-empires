//! Native OMP fork dispatch through `omp --fork`, child capture, and restart.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serial_test::parallel;

use crate::harness::{
    install_path_preserving_test_shell, require_tmux, session_by_title, wait_until,
    write_executable, TuiTestHarness,
};

const PARENT_ID: &str = "019342ab-1234-7def-8901-aaaaaaaaaaaa";
const CHILD_A_ID: &str = "019342ab-1234-7def-8901-bbbbbbbbbbbb";
const CHILD_B_ID: &str = "019342ab-1234-7def-8901-cccccccccccc";

/// A fake `omp` that keeps the store contract AoE relies on: a fresh launch
/// creates a session, `--fork <id>` copies that session from `--session-dir`
/// into a new one there, and `--resume <id>` reopens it. Each launch points the
/// pane's terminal breadcrumb at its session and records its argv.
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
    if ls "$dir"/*_CHILD_A.jsonl >/dev/null 2>&1; then sid=CHILD_B; else sid=CHILD_A; fi
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
"#
    .replace("CHILD_A", CHILD_A_ID)
    .replace("CHILD_B", CHILD_B_ID)
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

/// Wait until the row titled `title` holds a captured binding for `id`, and
/// return that binding's transcript path.
fn wait_for_binding(h: &TuiTestHarness, title: &str, id: &str) -> PathBuf {
    wait_until(Duration::from_secs(15), Duration::from_millis(100), || {
        let sessions = h.read_sessions();
        let row = session_by_title(&sessions, title);
        let binding = &row["agent_session_binding"];
        match (
            row["agent_session_id"].as_str(),
            binding["transcript_path"].as_str(),
        ) {
            (Some(actual), Some(path)) if actual == id && binding["session_id"] == id => {
                Ok(PathBuf::from(path))
            }
            _ => Err(format!("{title} has not captured {id}: {row}")),
        }
    })
}

fn body(transcript: &Path) -> Vec<String> {
    std::fs::read_to_string(transcript)
        .unwrap()
        .lines()
        .skip(2)
        .map(str::to_owned)
        .collect()
}

#[test]
#[parallel]
fn omp_fork_captures_each_child_and_restarts_without_reforking() {
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
    let parent_transcript = wait_for_binding(&h, "OmpParent", PARENT_ID);
    let parent_bytes = std::fs::read(&parent_transcript).unwrap();
    let parent_binding =
        session_by_title(&h.read_sessions(), "OmpParent")["agent_session_binding"].clone();

    let mut seen = BTreeSet::new();
    next_launch(&h, &mut seen);
    let mut child_transcripts = Vec::new();
    for (title, id) in [("OmpChildA", CHILD_A_ID), ("OmpChildB", CHILD_B_ID)] {
        h.run_cli_ok(&[
            "add",
            project.to_str().unwrap(),
            "-t",
            title,
            "--fork-from",
            "OmpParent",
        ]);
        let sessions = h.read_sessions();
        let child = session_by_title(&sessions, title);
        assert_eq!(child["tool"], "omp");
        assert_eq!(child["resume_intent"]["kind"], "Fork");

        h.run_cli_ok(&["session", "start", title]);
        let fork = &next_launch(&h, &mut seen);
        assert_eq!(flag_value(fork, "--fork"), Some(PARENT_ID));
        assert_eq!(
            std::fs::canonicalize(flag_value(fork, "--session-dir").unwrap()).unwrap(),
            std::fs::canonicalize(parent_transcript.parent().unwrap()).unwrap(),
            "the fork must be pinned to the parent's session directory"
        );
        assert_eq!(flag_value(fork, "--resume"), None);

        let child_transcript = wait_for_binding(&h, title, id);
        assert_ne!(child_transcript, parent_transcript);
        assert_eq!(body(&child_transcript), body(&parent_transcript));
        assert!(
            session_by_title(&h.read_sessions(), title)
                .get("resume_intent")
                .is_none(),
            "the one-shot fork intent must be consumed"
        );
        child_transcripts.push(child_transcript);
    }

    h.run_cli_ok(&["session", "stop", "OmpChildA"]);
    h.run_cli_ok(&["session", "start", "OmpChildA"]);
    let resume = &next_launch(&h, &mut seen);
    assert_eq!(flag_value(resume, "--resume"), Some(CHILD_A_ID));
    assert_eq!(flag_value(resume, "--fork"), None);
    assert_eq!(
        wait_for_binding(&h, "OmpChildA", CHILD_A_ID),
        child_transcripts[0]
    );

    let sessions = h.read_sessions();
    assert_eq!(
        session_by_title(&sessions, "OmpParent")["agent_session_binding"],
        parent_binding
    );
    assert_eq!(std::fs::read(&parent_transcript).unwrap(), parent_bytes);
}
