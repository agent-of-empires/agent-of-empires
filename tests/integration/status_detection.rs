//! Golden tests for status detection
//!
//! These tests verify that status detection works correctly against real
//! terminal captures. When a tool updates their TUI, these tests will fail
//! if the detection logic no longer works.
//!
//! Note: Claude Code and Cursor use hook-based detection (not tmux pane parsing),
//! so they have no fixture-based tests here.
//!
//! Each state is a directory containing one or more fixture files. This allows
//! users to submit additional screenshots for bug reports, and all examples
//! will be tested to ensure correct detection.
//!
//! To add fixtures after a bug report or tool update:
//! 1. Run: scripts/capture-fixtures.sh <tool> <state> <tmux_session> [description]
//! 2. Verify the new captures look correct
//! 3. Update detection logic if needed
//! 4. Re-run tests

use agent_of_empires::agents;
use agent_of_empires::session::Status;
use std::fs;
use std::path::PathBuf;

fn fixtures_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn strip_fixture_header(content: &str) -> String {
    content
        .lines()
        .filter(|line| !line.starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n")
}

fn test_all_fixtures_in_dir<F>(
    tool: &str,
    state: &str,
    expected: Status,
    preprocess: fn(String) -> String,
    detect_fn: F,
) where
    F: Fn(&str) -> Status,
{
    let dir = fixtures_path().join(tool).join(state);

    let entries: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("Failed to read fixture directory {:?}: {}", dir, e))
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .path()
                .extension()
                .map(|ext| ext == "txt")
                .unwrap_or(false)
        })
        .collect();

    assert!(
        !entries.is_empty(),
        "No fixture files found in {:?}. Add at least one .txt fixture file.",
        dir
    );

    for entry in entries {
        let path = entry.path();
        let raw_content = fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read fixture {:?}: {}", path, e));
        let content = preprocess(strip_fixture_header(&raw_content));
        let status = detect_fn(&content);

        assert_eq!(
            status,
            expected,
            "Fixture {:?} should detect as {:?}, but got {:?}.\n\
             Fixture content:\n{}\n\n\
             If the tool changed their TUI, update the detection logic in src/tmux/session.rs",
            path.file_name().unwrap(),
            expected,
            status,
            content
        );
    }
}

fn identity(s: String) -> String {
    s
}

mod opencode {
    use super::*;

    fn detect(content: &str) -> Status {
        let agent = agents::get_agent("opencode").unwrap();
        (agent.detect_status)(content)
    }

    #[test]
    #[serial_test::parallel]
    fn test_running_state() {
        test_all_fixtures_in_dir("opencode", "running", Status::Running, identity, detect);
    }

    #[test]
    #[serial_test::parallel]
    fn test_waiting_permission_state() {
        test_all_fixtures_in_dir(
            "opencode",
            "waiting_permission",
            Status::Waiting,
            identity,
            detect,
        );
    }

    #[test]
    #[serial_test::parallel]
    fn test_idle_state() {
        test_all_fixtures_in_dir("opencode", "idle", Status::Idle, identity, detect);
    }
}
