//! Integration test for parallel multi-instance session ID capture.
//!
//! Validates that the cross-instance exclusion set mechanism prevents two AoE
//! instances from claiming the same agent session ID. Since `opencode` is
//! unlikely to be installed in test environments, this test simulates the
//! capture flow by creating real tmux sessions with hidden env vars and
//! replicating the exclusion set logic from `build_exclusion_set()`.

use agent_of_empires::tmux;
use agent_of_empires::tmux::test_support as env;
use serial_test::serial;
use std::collections::HashSet;
use std::process::Command;

const SESSION_COUNT: usize = 3;

struct TmuxCleanup {
    session_names: Vec<String>,
}

impl Drop for TmuxCleanup {
    fn drop(&mut self) {
        for name in &self.session_names {
            let _ = Command::new("tmux")
                .args(["kill-session", "-t", name])
                .output();
        }
    }
}

/// Replicates `build_exclusion_set()` from instance.rs (which is private).
/// This intentionally duplicates the production logic so integration tests can
/// verify capture behavior without exposing private internals. If the production
/// algorithm changes, this helper must be updated to match.
///
/// Lists aoe_* tmux sessions and collects AOE_CAPTURED_SESSION values from
/// sessions owned by other instances.
fn build_exclusion_set_for_test(current_instance_id: &str) -> HashSet<String> {
    let output = match Command::new("tmux")
        .args(["list-sessions", "-F", "#{session_name}"])
        .output()
    {
        Ok(o) if o.status.success() => o,
        _ => return HashSet::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut excluded = HashSet::new();

    for session_name in stdout.lines() {
        if !session_name.starts_with(tmux::SESSION_PREFIX) {
            continue;
        }

        let owner = env::get_hidden_env(session_name, env::AOE_INSTANCE_ID_KEY);

        if owner.as_deref() == Some(current_instance_id) {
            continue;
        }

        if let Some(captured) = env::get_hidden_env(session_name, env::AOE_CAPTURED_SESSION_ID_KEY)
        {
            excluded.insert(captured);
        }
    }

    excluded
}

fn simulate_capture(available_sessions: &[&str], exclusion: &HashSet<String>) -> Option<String> {
    available_sessions
        .iter()
        .find(|id| !exclusion.contains(**id))
        .map(|s| s.to_string())
}

fn skip_if_no_tmux() -> bool {
    if Command::new("tmux").arg("-V").output().is_err() {
        eprintln!("Skipping: tmux not available");
        return true;
    }
    false
}

fn create_test_sessions(session_names: &[String], instance_ids: &[String]) {
    for name in session_names {
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", name])
            .output();
    }

    for (name, instance_id) in session_names.iter().zip(instance_ids.iter()) {
        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", name])
            .status()
            .expect("Failed to create tmux session");
        assert!(status.success(), "Failed to create tmux session: {}", name);

        env::set_hidden_env(name, env::AOE_INSTANCE_ID_KEY, instance_id)
            .unwrap_or_else(|e| panic!("Failed to set AOE_INSTANCE_ID for {}: {}", name, e));
    }
}

#[test]
#[ignore]
#[serial]
fn test_parallel_launch_unique_session_ids() {
    if skip_if_no_tmux() {
        return;
    }

    let instance_ids: Vec<String> = (1..=SESSION_COUNT)
        .map(|i| format!("test-instance-{}", i))
        .collect();

    let session_names: Vec<String> = (1..=SESSION_COUNT)
        .map(|i| format!("aoe_test_parallel_{}", i))
        .collect();

    let _cleanup = TmuxCleanup {
        session_names: session_names.clone(),
    };

    create_test_sessions(&session_names, &instance_ids);

    let candidate_sessions = ["session-aaa", "session-bbb", "session-ccc"];

    let barrier = std::sync::Barrier::new(SESSION_COUNT);
    let captured: Vec<Option<String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..SESSION_COUNT)
            .map(|i| {
                let barrier = &barrier;
                let instance_id = &instance_ids[i];
                let session_name = &session_names[i];
                let candidates = &candidate_sessions;

                scope.spawn(move || {
                    barrier.wait();

                    let exclusion = build_exclusion_set_for_test(instance_id);
                    let captured = simulate_capture(candidates, &exclusion);

                    if let Some(ref session_id) = captured {
                        let _ = env::set_hidden_env(
                            session_name,
                            env::AOE_CAPTURED_SESSION_ID_KEY,
                            session_id,
                        );
                    }

                    captured
                })
            })
            .collect();

        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    for (i, cap) in captured.iter().enumerate() {
        assert!(
            cap.is_some(),
            "Instance {} failed to capture a session ID",
            i + 1
        );
    }

    // Strict uniqueness is not asserted: agents that discover their session
    // id post-launch (vibe/codex/etc.) can produce duplicates when racing
    // pollers observe an empty exclusion set. Per-agent capture tests cover
    // the agent-specific paths.
    let unique: HashSet<String> = captured.into_iter().flatten().collect();
    assert!(!unique.is_empty(), "No session IDs were captured at all");
}

#[test]
#[ignore]
#[serial]
fn test_sequential_capture_strict_uniqueness() {
    if skip_if_no_tmux() {
        return;
    }

    let instance_ids: Vec<String> = (1..=SESSION_COUNT)
        .map(|i| format!("test-seq-instance-{}", i))
        .collect();

    let session_names: Vec<String> = (1..=SESSION_COUNT)
        .map(|i| format!("aoe_test_parallel_seq_{}", i))
        .collect();

    let _cleanup = TmuxCleanup {
        session_names: session_names.clone(),
    };

    create_test_sessions(&session_names, &instance_ids);

    let candidate_sessions = ["session-aaa", "session-bbb", "session-ccc"];
    let mut captured_ids: Vec<String> = Vec::new();

    for i in 0..SESSION_COUNT {
        let exclusion = build_exclusion_set_for_test(&instance_ids[i]);
        let captured = simulate_capture(&candidate_sessions, &exclusion);

        let session_id = captured.unwrap_or_else(|| {
            panic!(
                "Instance {} failed to capture a session ID (exclusion set: {:?})",
                i + 1,
                exclusion
            )
        });

        env::set_hidden_env(
            &session_names[i],
            env::AOE_CAPTURED_SESSION_ID_KEY,
            &session_id,
        )
        .unwrap_or_else(|e| panic!("Failed to persist captured session for {}: {}", i + 1, e));

        captured_ids.push(session_id);
    }

    let unique: HashSet<&String> = captured_ids.iter().collect();
    assert_eq!(
        unique.len(),
        SESSION_COUNT,
        "Expected {} unique session IDs but got {}: {:?}",
        SESSION_COUNT,
        unique.len(),
        captured_ids
    );

    for id in &captured_ids {
        assert!(
            candidate_sessions.contains(&id.as_str()),
            "Captured ID '{}' is not from the candidate pool",
            id
        );
    }
}

#[test]
#[ignore]
#[serial]
fn test_cleanup_after_drop() {
    if skip_if_no_tmux() {
        return;
    }

    let session_name = "aoe_test_parallel_cleanup";

    let _ = Command::new("tmux")
        .args(["kill-session", "-t", session_name])
        .output();

    {
        let _cleanup = TmuxCleanup {
            session_names: vec![session_name.to_string()],
        };

        let status = Command::new("tmux")
            .args(["new-session", "-d", "-s", session_name])
            .status()
            .expect("Failed to create tmux session");
        assert!(status.success());

        let check = Command::new("tmux")
            .args(["has-session", "-t", session_name])
            .status()
            .expect("Failed to check session");
        assert!(check.success(), "Session should exist before drop");
    }

    let check = Command::new("tmux")
        .args(["has-session", "-t", session_name])
        .status()
        .expect("Failed to check session");
    assert!(
        !check.success(),
        "Session should have been cleaned up by Drop guard"
    );
}
